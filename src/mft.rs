//! Indexes an NTFS volume by reading its Master File Table (MFT) directly.
//!
//! The MFT holds one record per file and folder: names, parent folder,
//! timestamps, attributes, and sizes. Reading it in large blocks is much
//! faster than opening every folder, because the volume is read almost
//! sequentially and no per-folder system calls are made. Reading a raw volume
//! needs administrator privileges on Windows. [`crate::scan`] uses this module
//! for drive roots and falls back to the directory walk when it fails.
//!
//! The parser works on any `Read + Seek` source, so tests can use an NTFS
//! image file.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use rayon::prelude::*;

use crate::model::Entry;

/// Record of the volume's root folder.
const ROOT_RECORD: u64 = 5;
/// Records below this number are NTFS metadata files such as `$MFT`.
const FIRST_USER_RECORD: u64 = 16;
/// Bytes read per block. A multiple of every valid cluster and record size.
const BLOCK_BYTES: u64 = 4 << 20;
/// Alignment of read buffers. Raw volume reads bypass the file cache, so
/// buffers must be aligned to the sector size; a page covers every disk.
const BUFFER_ALIGN: usize = 4096;
/// The low 48 bits of a file reference are the record number.
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Fixups protect every 512-byte stride of a record, whatever the sector size.
const FIXUP_STRIDE: usize = 512;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;
const NAMESPACE_DOS: u8 = 2;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `$FILE_NAME` index flags that are not Windows file attributes.
const NON_API_ATTRIBUTES: u32 = 0x3000_0000;

/// Indexes a drive root such as `C:\` through its MFT.
///
/// Fails when `root` is not a drive root, the volume is not NTFS, or the raw
/// volume cannot be opened (for example, without administrator privileges).
#[cfg(windows)]
pub fn scan_volume_root(root: &Path) -> Result<Vec<Entry>> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;

    let letter = drive_root_letter(root).context("not a drive root")?;
    // Keep the caller's spelling, such as `c:\`, so paths match the ones the
    // file watcher reports for the same root.
    let text = root.to_str().context("not a drive root")?;
    let display = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text));
    let device = format!(r"\\.\{letter}:");
    let open = || {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&device)
    };
    let readers = if incurs_seek_penalty(&device) { 1 } else { 8 };
    read_volume(open, readers, &display)
}

/// Whether the disk under a volume is a hard disk. SSDs report no seek
/// penalty. Unknown disks count as SSDs, which are the common case.
#[cfg(windows)]
fn incurs_seek_penalty(device: &str) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    // Querying the property needs no read access, so no extra privileges.
    let Ok(volume) = std::fs::OpenOptions::new().access_mode(0).share_mode(0x7).open(device) else {
        return false;
    };
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut answer = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned = 0;
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle() as _,
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const query).cast(),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            (&raw mut answer).cast(),
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    ok != 0 && returned as usize >= std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() && answer.IncursSeekPenalty
}

/// The drive letter of a root path such as `C:\` or `\\?\C:\`.
pub fn drive_root_letter(root: &Path) -> Option<char> {
    let text = root.to_str()?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let mut chars = text.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':') && matches!(chars.as_str(), "\\" | "/"))
        .then(|| letter.to_ascii_uppercase())
}

/// Reads every file and folder of an NTFS volume.
///
/// `open` returns a new reader positioned anywhere on the raw volume.
/// `root` is the display path of the volume root, for example `C:\\`. The
/// root itself is not returned, as with [`crate::scan::scan`].
///
/// Raw volume reads do not use the file cache, so every scan reads the MFT
/// from the disk. `readers` threads take blocks in disk order: use one on a
/// hard disk, where parallel reads cause seeks, and several on an SSD, which
/// is fastest with many reads in flight. Other threads parse the blocks
/// already read. Blocks without records in use, according to the `$MFT`
/// bitmap, are skipped.
pub fn read_volume<R, F>(open: F, readers: usize, root: &Path) -> Result<Vec<Entry>>
where
    R: Read + Seek + Send,
    F: Fn() -> io::Result<R> + Sync,
{
    let mut volume = open().context("cannot open the volume")?;
    let layout = Layout::load(&mut volume)?;

    let record_size = layout.geometry.record_size;
    let read_end = layout
        .allocated_bytes()
        .min(round_up(layout.data_size, layout.geometry.cluster_size));
    // Only records that are actually allocated can be read.
    let records = layout.data_size.min(read_end) / record_size;
    ensure!(records <= u64::from(u32::MAX), "the MFT has too many records");
    let records_per_block = BLOCK_BYTES / record_size;
    let block_count = usize::try_from(records.div_ceil(records_per_block)).context("the MFT is too large")?;
    let in_use = |number: u64| {
        layout.bitmap.as_ref().is_none_or(|bitmap| {
            bitmap
                .get((number / 8) as usize)
                .is_none_or(|byte| byte & (1 << (number % 8)) != 0)
        })
    };
    let wanted: Vec<usize> = (0..block_count)
        .filter(|&block| {
            let first = block as u64 * records_per_block;
            let last = (first + records_per_block).min(records);
            match &layout.bitmap {
                Some(bitmap) if first % 8 == 0 => {
                    let bytes = (first / 8) as usize..(last.div_ceil(8)) as usize;
                    bitmap.get(bytes).is_none_or(|bytes| bytes.iter().any(|&byte| byte != 0))
                }
                _ => (first..last).any(in_use),
            }
        })
        .collect();

    let readers = readers.clamp(1, wanted.len().max(1));
    if std::env::var_os("EVERYRUST_DEBUG_SCAN").is_some() {
        eprintln!(
            "MFT: {} MiB, reading {} of {} blocks, bitmap {}, {readers} reader(s)",
            read_end >> 20,
            wanted.len(),
            block_count,
            if layout.bitmap.is_some() { "used" } else { "missing" },
        );
    }
    let pool_size = (rayon::current_num_threads() + readers * 2).clamp(4, 32);
    let (to_parse, parse_queue) = std::sync::mpsc::sync_channel::<(usize, AlignedBuffer)>(pool_size / 2);
    let (recycle, recycled) = std::sync::mpsc::channel::<AlignedBuffer>();
    let recycled = std::sync::Mutex::new(recycled);
    let allocated = std::sync::atomic::AtomicUsize::new(0);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    let layout = &layout;
    let wanted = &wanted;
    let mut first_volume = Some(volume);

    let (parsed, read_results) = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..readers)
            .map(|_| {
                let volume = first_volume.take();
                let to_parse = to_parse.clone();
                let (recycled, allocated, next, failed, open) = (&recycled, &allocated, &next, &failed, &open);
                scope.spawn(move || -> Result<()> {
                    use std::sync::atomic::Ordering::Relaxed;
                    let mut volume = match volume {
                        Some(volume) => volume,
                        None => open().context("cannot open the volume")?,
                    };
                    let result = (|| {
                        loop {
                            let Some(&block) = wanted.get(next.fetch_add(1, Relaxed)) else { return Ok(()) };
                            if failed.load(Relaxed) {
                                return Ok(());
                            }
                            let reused = recycled.lock().unwrap().try_recv().ok();
                            let mut buffer = match reused {
                                Some(buffer) => buffer,
                                None if allocated.fetch_add(1, Relaxed) < pool_size => AlignedBuffer::default(),
                                None => match recycled.lock().unwrap().recv() {
                                    Ok(buffer) => buffer,
                                    Err(_) => return Ok(()),
                                },
                            };
                            let start = block as u64 * BLOCK_BYTES;
                            buffer.resize(((read_end - start).min(BLOCK_BYTES)) as usize);
                            layout
                                .read(&mut volume, start, &mut buffer)
                                .with_context(|| format!("cannot read MFT block at offset {start}"))?;
                            if to_parse.send((block, buffer)).is_err() {
                                return Ok(());
                            }
                        }
                    })();
                    if result.is_err() {
                        failed.store(true, Relaxed);
                    }
                    result
                })
            })
            .collect();
        drop(to_parse);
        let parsed: Vec<(usize, Block)> = parse_queue
            .into_iter()
            .par_bridge()
            .map(|(block, mut buffer)| {
                let first = block as u64 * records_per_block;
                let mut parsed = Block::default();
                for (offset, record) in buffer.chunks_exact_mut(record_size as usize).enumerate() {
                    let number = first + offset as u64;
                    if number >= records {
                        break;
                    }
                    if !in_use(number) {
                        continue;
                    }
                    if let Some(record) = parse_record(number, record, block as u32, &mut parsed.names) {
                        parsed.records.push(record);
                    }
                }
                let _ = recycle.send(buffer);
                (block, parsed)
            })
            .collect();
        drop(recycle);
        let read_results: Vec<Result<()>> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect();
        (parsed, read_results)
    });
    read_results.into_iter().collect::<Result<()>>()?;

    let mut blocks: Vec<Block> = (0..block_count).map(|_| Block::default()).collect();
    for (index, block) in parsed {
        blocks[index] = block;
    }
    build_entries(blocks, records, records_per_block, root)
}

/// A zeroed byte buffer whose start is aligned to [`BUFFER_ALIGN`].
#[derive(Default)]
struct AlignedBuffer {
    pages: Vec<Page>,
    length: usize,
}

#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct Page([u8; BUFFER_ALIGN]);

impl AlignedBuffer {
    fn new(length: usize) -> Self {
        let mut buffer = Self::default();
        buffer.resize(length);
        buffer
    }

    fn resize(&mut self, length: usize) {
        self.pages.resize(length.div_ceil(BUFFER_ALIGN), Page([0; BUFFER_ALIGN]));
        self.length = length;
    }
}

impl std::ops::Deref for AlignedBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `Page` is plain bytes without padding, the pages are
        // contiguous, and `length` never exceeds their total size.
        unsafe { std::slice::from_raw_parts(self.pages.as_ptr().cast(), self.length) }
    }
}

impl std::ops::DerefMut for AlignedBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `deref`, and the borrow is unique.
        unsafe { std::slice::from_raw_parts_mut(self.pages.as_mut_ptr().cast(), self.length) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    cluster_size: u64,
    record_size: u64,
    mft_cluster: u64,
}

fn parse_boot_sector(boot: &[u8]) -> Result<Geometry> {
    ensure!(boot.get(3..11) == Some(b"NTFS    "), "not an NTFS volume");
    let sector_size = u64::from(le_u16(boot, 0x0B).context("short boot sector")?);
    ensure!(
        sector_size.is_power_of_two() && (256..=4096).contains(&sector_size),
        "invalid NTFS sector size {sector_size}"
    );
    let raw = *boot.get(0x0D).context("short boot sector")?;
    let sectors_per_cluster = if raw <= 0x80 {
        u64::from(raw)
    } else {
        // Large clusters store a negative power of two.
        1u64 << (256 - u32::from(raw)).min(31)
    };
    let cluster_size = sector_size * sectors_per_cluster;
    ensure!(
        cluster_size.is_power_of_two() && cluster_size <= 2 << 20,
        "invalid NTFS cluster size {cluster_size}"
    );
    let mft_cluster = le_u64(boot, 0x30).context("short boot sector")?;
    let raw = *boot.get(0x40).context("short boot sector")? as i8;
    let record_size = if raw > 0 {
        u64::from(raw as u8) * cluster_size
    } else {
        let shift = -i32::from(raw);
        ensure!((9..=16).contains(&shift), "invalid MFT record size 2^{shift}");
        1u64 << shift
    };
    ensure!(
        record_size.is_power_of_two() && (512..=65536).contains(&record_size),
        "invalid MFT record size {record_size}"
    );
    Ok(Geometry {
        cluster_size,
        record_size,
        mft_cluster,
    })
}

/// One contiguous piece of a non-resident attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    vcn: u64,
    /// `None` for a sparse run, which reads as zeros.
    lcn: Option<u64>,
    clusters: u64,
}

/// Where the MFT itself lives on the volume.
struct Layout {
    geometry: Geometry,
    runs: Vec<Run>,
    data_size: u64,
    /// One bit per MFT record that is in use; `None` when it cannot be read.
    bitmap: Option<Vec<u8>>,
}

impl Layout {
    fn load<R: Read + Seek>(volume: &mut R) -> Result<Self> {
        // 4 KiB covers the boot sector on 512-byte and 4 KiB sector disks.
        let mut boot = AlignedBuffer::new(4096);
        volume.seek(SeekFrom::Start(0))?;
        volume.read_exact(&mut boot).context("cannot read the boot sector")?;
        let geometry = parse_boot_sector(&boot)?;

        // Record 0 describes the MFT. Read it from the cluster in the boot sector.
        let mut first = AlignedBuffer::new(round_up(geometry.record_size, geometry.cluster_size) as usize);
        volume.seek(SeekFrom::Start(
            geometry
                .mft_cluster
                .checked_mul(geometry.cluster_size)
                .context("invalid MFT location")?,
        ))?;
        volume.read_exact(&mut first).context("cannot read the first MFT record")?;
        let record = &mut first[..geometry.record_size as usize];
        ensure!(apply_fixups(record), "the first MFT record is damaged");

        let mut segments = Vec::new();
        let mut bitmap_segments = Vec::new();
        collect_segments(record, ATTR_DATA, &mut segments)?;
        let mut bitmap = collect_bitmap(record, &mut bitmap_segments);
        let mut layout = Self::from_segments(geometry, &segments)?;

        // A fragmented MFT can keep more runs in extension records listed by
        // its $ATTRIBUTE_LIST.
        let extensions = layout.attribute_list_extensions(volume, record)?;
        for number in extensions {
            let mut extension = AlignedBuffer::new(geometry.record_size as usize);
            let offset = number
                .checked_mul(geometry.record_size)
                .context("invalid MFT extension record")?;
            layout.read(volume, offset, &mut extension)?;
            ensure!(apply_fixups(&mut extension), "MFT extension record {number} is damaged");
            collect_segments(&extension, ATTR_DATA, &mut segments)?;
            bitmap = bitmap.or_else(|| collect_bitmap(&extension, &mut bitmap_segments));
        }
        if !segments.is_empty() {
            layout = Self::from_segments(geometry, &segments)?;
        }
        // The bitmap only speeds up reading; without it every record is read.
        layout.bitmap = bitmap.or_else(|| layout.read_bitmap(volume, &bitmap_segments).ok());
        Ok(layout)
    }

    /// Reads a non-resident `$MFT` bitmap from its segments.
    fn read_bitmap<R: Read + Seek>(&self, volume: &mut R, segments: &[(u64, Vec<Run>, Option<u64>)]) -> Result<Vec<u8>> {
        let size = segments
            .iter()
            .find_map(|(vcn, _, size)| (*vcn == 0).then_some(*size).flatten())
            .context("no MFT bitmap")?;
        let mut ordered: Vec<_> = segments.iter().collect();
        ordered.sort_by_key(|(vcn, _, _)| *vcn);
        ordered.dedup_by_key(|(vcn, _, _)| *vcn);
        let runs: Vec<Run> = ordered.into_iter().flat_map(|(_, runs, _)| runs.iter().copied()).collect();
        validate_runs(&runs, self.geometry.cluster_size)?;
        let size = usize::try_from(size).context("MFT bitmap is too large")?;
        ensure!(size <= 1 << 30, "MFT bitmap is too large");
        // Raw volume reads must cover whole sectors, so read whole clusters.
        let mut bytes = AlignedBuffer::new(round_up(size as u64, self.geometry.cluster_size) as usize);
        read_runs(volume, &runs, self.geometry.cluster_size, 0, &mut bytes)?;
        Ok(bytes[..size].to_vec())
    }

    fn from_segments(geometry: Geometry, segments: &[(u64, Vec<Run>, Option<u64>)]) -> Result<Self> {
        let data_size = segments
            .iter()
            .find_map(|(vcn, _, size)| (*vcn == 0).then_some(*size).flatten())
            .context("the MFT has no data attribute")?;
        let mut ordered: Vec<_> = segments.iter().collect();
        ordered.sort_by_key(|(vcn, _, _)| *vcn);
        ordered.dedup_by_key(|(vcn, _, _)| *vcn);
        let runs: Vec<Run> = ordered.into_iter().flat_map(|(_, runs, _)| runs.iter().copied()).collect();
        ensure!(
            runs.windows(2).all(|pair| pair[0].vcn + pair[0].clusters == pair[1].vcn),
            "the MFT runs are not contiguous"
        );
        validate_runs(&runs, geometry.cluster_size)?;
        Ok(Self {
            geometry,
            runs,
            data_size,
            bitmap: None,
        })
    }

    fn allocated_bytes(&self) -> u64 {
        self.runs
            .last()
            .map_or(0, |run| (run.vcn + run.clusters) * self.geometry.cluster_size)
    }

    /// Extension records that hold more of the MFT's own data runs or bitmap.
    fn attribute_list_extensions<R: Read + Seek>(&self, volume: &mut R, record: &[u8]) -> Result<Vec<u64>> {
        let Some(list) = attributes(record).find(|attribute| attribute.kind == ATTR_ATTRIBUTE_LIST) else {
            return Ok(Vec::new());
        };
        let bytes = if let Some(value) = list.resident_value() {
            value.to_vec()
        } else {
            let header = list.non_resident().context("damaged MFT attribute list")?;
            let size = usize::try_from(header.data_size).context("MFT attribute list is too large")?;
            ensure!(size <= 16 << 20, "MFT attribute list is too large");
            validate_runs(&header.runs, self.geometry.cluster_size)?;
            // Raw volume reads must cover whole sectors, so read whole clusters.
            let mut bytes = AlignedBuffer::new(round_up(size as u64, self.geometry.cluster_size) as usize);
            read_runs(volume, &header.runs, self.geometry.cluster_size, 0, &mut bytes)?;
            bytes[..size].to_vec()
        };
        let mut numbers = Vec::new();
        let mut offset = 0;
        while let (Some(kind), Some(length)) = (le_u32(&bytes, offset), le_u16(&bytes, offset + 4)) {
            let length = usize::from(length);
            if length < 0x1A {
                break;
            }
            let name_length = bytes.get(offset + 6).copied().unwrap_or(0);
            let number = le_u64(&bytes, offset + 0x10).unwrap_or(0) & RECORD_MASK;
            if matches!(kind, ATTR_DATA | ATTR_BITMAP)
                && name_length == 0
                && number != 0
                && !numbers.contains(&number)
            {
                numbers.push(number);
            }
            offset += length;
        }
        Ok(numbers)
    }

    /// Reads `buffer.len()` bytes of MFT data that start at `offset`.
    fn read<R: Read + Seek>(&self, volume: &mut R, offset: u64, buffer: &mut [u8]) -> Result<()> {
        read_runs(volume, &self.runs, self.geometry.cluster_size, offset, buffer)
    }
}

/// Checks that every byte offset of the runs fits in a `u64`, so later
/// offset arithmetic cannot overflow.
fn validate_runs(runs: &[Run], cluster_size: u64) -> Result<()> {
    for run in runs {
        let end_vcn = run.vcn.checked_add(run.clusters);
        let end_lcn = run.lcn.map_or(Some(0), |lcn| lcn.checked_add(run.clusters));
        ensure!(
            end_vcn.and_then(|end| end.checked_mul(cluster_size)).is_some()
                && end_lcn.and_then(|end| end.checked_mul(cluster_size)).is_some(),
            "data run outside the volume"
        );
    }
    Ok(())
}

fn read_runs<R: Read + Seek>(
    volume: &mut R,
    runs: &[Run],
    cluster_size: u64,
    offset: u64,
    buffer: &mut [u8],
) -> Result<()> {
    let mut done = 0;
    while done < buffer.len() {
        let position = offset + done as u64;
        let vcn = position / cluster_size;
        let index = runs.partition_point(|run| run.vcn + run.clusters <= vcn);
        let run = runs
            .get(index)
            .filter(|run| run.vcn <= vcn)
            .with_context(|| format!("offset {position} is outside the attribute"))?;
        let run_end = (run.vcn + run.clusters) * cluster_size;
        let length = (buffer.len() - done).min((run_end - position) as usize);
        let target = &mut buffer[done..done + length];
        match run.lcn {
            None => target.fill(0),
            Some(lcn) => {
                volume.seek(SeekFrom::Start(lcn * cluster_size + position - run.vcn * cluster_size))?;
                volume.read_exact(target)?;
            }
        }
        done += length;
    }
    Ok(())
}

/// Adds the non-resident segments of the unnamed attribute `kind` of an MFT
/// record: (first VCN, runs, size).
fn collect_segments(record: &[u8], kind: u32, segments: &mut Vec<(u64, Vec<Run>, Option<u64>)>) -> Result<()> {
    for attribute in attributes(record) {
        if attribute.kind != kind || attribute.name_length != 0 {
            continue;
        }
        let header = attribute.non_resident().context("damaged MFT attribute")?;
        let size = (header.first_vcn == 0).then_some(header.data_size);
        segments.push((header.first_vcn, header.runs, size));
    }
    Ok(())
}

/// Returns a resident `$MFT` bitmap, or adds the segments of a non-resident one.
fn collect_bitmap(record: &[u8], segments: &mut Vec<(u64, Vec<Run>, Option<u64>)>) -> Option<Vec<u8>> {
    let attribute = attributes(record).find(|attribute| attribute.kind == ATTR_BITMAP && attribute.name_length == 0)?;
    if let Some(value) = attribute.resident_value() {
        return Some(value.to_vec());
    }
    let header = attribute.non_resident()?;
    let size = (header.first_vcn == 0).then_some(header.data_size);
    segments.push((header.first_vcn, header.runs, size));
    None
}

/// Undoes the update sequence fixups of a record. False for a damaged or unused record.
fn apply_fixups(record: &mut [u8]) -> bool {
    if record.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(offset), Some(count)) = (le_u16(record, 4), le_u16(record, 6)) else {
        return false;
    };
    let (offset, count) = (usize::from(offset), usize::from(count));
    if count < 2 || (count - 1) * FIXUP_STRIDE > record.len() || offset + count * 2 > record.len() {
        return false;
    }
    let check = [record[offset], record[offset + 1]];
    for index in 1..count {
        let end = index * FIXUP_STRIDE;
        if record[end - 2..end] != check {
            return false;
        }
        record[end - 2] = record[offset + index * 2];
        record[end - 1] = record[offset + index * 2 + 1];
    }
    true
}

fn parse_runs(bytes: &[u8], first_vcn: u64) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut position = 0;
    let mut lcn: i64 = 0;
    let mut vcn = first_vcn;
    loop {
        let header = *bytes.get(position)?;
        if header == 0 {
            return Some(runs);
        }
        let length_size = usize::from(header & 0x0F);
        let offset_size = usize::from(header >> 4);
        if length_size == 0 || length_size > 8 || offset_size > 8 {
            return None;
        }
        position += 1;
        let clusters = read_unsigned(bytes.get(position..position + length_size)?);
        position += length_size;
        let run_lcn = if offset_size == 0 {
            None
        } else {
            lcn = lcn.checked_add(read_signed(bytes.get(position..position + offset_size)?))?;
            position += offset_size;
            Some(u64::try_from(lcn).ok()?)
        };
        if clusters == 0 {
            return None;
        }
        runs.push(Run {
            vcn,
            lcn: run_lcn,
            clusters,
        });
        vcn = vcn.checked_add(clusters)?;
    }
}

fn read_unsigned(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |value, &byte| value << 8 | u64::from(byte))
}

fn read_signed(bytes: &[u8]) -> i64 {
    let value = read_unsigned(bytes);
    let unused = 64 - 8 * bytes.len() as u32;
    ((value << unused) as i64) >> unused
}

struct Attribute<'a> {
    kind: u32,
    name_length: u8,
    bytes: &'a [u8],
}

struct NonResident {
    first_vcn: u64,
    data_size: u64,
    runs: Vec<Run>,
}

impl<'a> Attribute<'a> {
    fn resident_value(&self) -> Option<&'a [u8]> {
        if self.bytes.get(8) != Some(&0) {
            return None;
        }
        let length = le_u32(self.bytes, 0x10)? as usize;
        let offset = usize::from(le_u16(self.bytes, 0x14)?);
        self.bytes.get(offset..offset.checked_add(length)?)
    }

    fn non_resident(&self) -> Option<NonResident> {
        if self.bytes.get(8) != Some(&1) {
            return None;
        }
        let first_vcn = le_u64(self.bytes, 0x10)?;
        let runs_offset = usize::from(le_u16(self.bytes, 0x20)?);
        Some(NonResident {
            first_vcn,
            data_size: le_u64(self.bytes, 0x30)?,
            runs: parse_runs(self.bytes.get(runs_offset..)?, first_vcn)?,
        })
    }

    /// The logical size of a stream, read from either attribute form.
    fn stream_size(&self) -> Option<(u64, u64)> {
        match self.bytes.get(8)? {
            0 => Some((0, self.resident_value()?.len() as u64)),
            _ => Some((le_u64(self.bytes, 0x10)?, le_u64(self.bytes, 0x30)?)),
        }
    }
}

fn attributes(record: &[u8]) -> impl Iterator<Item = Attribute<'_>> {
    let end = le_u32(record, 0x18).map_or(0, |used| (used as usize).min(record.len()));
    let mut offset = le_u16(record, 0x14).map_or(usize::MAX, usize::from);
    std::iter::from_fn(move || {
        let kind = le_u32(record, offset).filter(|&kind| kind != ATTR_END)?;
        let length = le_u32(record, offset + 4)? as usize;
        if length < 0x18 || offset + length > end {
            return None;
        }
        let bytes = &record[offset..offset + length];
        offset += length;
        Some(Attribute {
            kind,
            name_length: bytes[9],
            bytes,
        })
    })
}

/// The records of one block, with the UTF-16 names they refer to.
#[derive(Default)]
struct Block {
    records: Vec<FileRecord>,
    names: Vec<u16>,
}

/// The parts of one MFT record that the index needs. Names point into the
/// arena of a [`Block`], so parsing makes no allocation per record.
struct FileRecord {
    number: u64,
    /// File reference of the base record; 0 for a base record.
    base: u64,
    times: Option<StandardInformation>,
    /// Size from the unnamed $DATA stream.
    data_size: Option<u64>,
    /// Size copied into $FILE_NAME; used when $DATA is missing.
    name_size: Option<u64>,
    first_name: Option<FileName>,
    /// Hard links after the first name; usually empty.
    more_names: Vec<FileName>,
    sequence: u16,
    is_dir: bool,
}

impl FileRecord {
    fn names(&self) -> impl Iterator<Item = &FileName> {
        self.first_name.iter().chain(&self.more_names)
    }

    fn name(&self, index: usize) -> &FileName {
        if index == 0 {
            self.first_name.as_ref().expect("first name")
        } else {
            &self.more_names[index - 1]
        }
    }

    fn add_name(&mut self, name: FileName) {
        if self.first_name.is_none() {
            self.first_name = Some(name);
        } else {
            self.more_names.push(name);
        }
    }
}

#[derive(Clone, Copy)]
struct StandardInformation {
    created: u64,
    modified: u64,
    attributes: u32,
}

#[derive(Clone, Copy)]
struct FileName {
    /// File reference of the parent folder.
    parent: u64,
    block: u32,
    start: u32,
    length: u8,
}

fn parse_record(number: u64, record: &mut [u8], block: u32, names: &mut Vec<u16>) -> Option<FileRecord> {
    if !apply_fixups(record) {
        return None;
    }
    let flags = le_u16(record, 0x16)?;
    if flags & RECORD_IN_USE == 0 {
        return None;
    }
    let mut file = FileRecord {
        number,
        base: le_u64(record, 0x20)?,
        times: None,
        data_size: None,
        name_size: None,
        first_name: None,
        more_names: Vec::new(),
        sequence: le_u16(record, 0x10)?,
        is_dir: flags & RECORD_IS_DIRECTORY != 0,
    };
    for attribute in attributes(record) {
        match attribute.kind {
            ATTR_STANDARD_INFORMATION => {
                let Some(value) = attribute.resident_value() else { continue };
                if let (Some(created), Some(modified), Some(attributes)) =
                    (le_u64(value, 0), le_u64(value, 8), le_u32(value, 0x20))
                {
                    file.times = Some(StandardInformation {
                        created,
                        modified,
                        attributes,
                    });
                }
            }
            ATTR_FILE_NAME => {
                let Some(value) = attribute.resident_value() else { continue };
                let (Some(parent), Some(&length), Some(&namespace)) =
                    (le_u64(value, 0), value.get(0x40), value.get(0x41))
                else {
                    continue;
                };
                // A short 8.3 alias duplicates the long name.
                if namespace == NAMESPACE_DOS {
                    continue;
                }
                let Some(bytes) = value.get(0x42..0x42 + 2 * usize::from(length)) else { continue };
                let start = names.len() as u32;
                names.extend(bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])));
                file.name_size = file.name_size.or(le_u64(value, 0x30));
                file.add_name(FileName {
                    parent,
                    block,
                    start,
                    length,
                });
            }
            ATTR_DATA if attribute.name_length == 0 => {
                if let Some((0, size)) = attribute.stream_size() {
                    file.data_size = Some(size);
                }
            }
            _ => {}
        }
    }
    Some(file)
}

/// Finds records by number. Block `b` holds records
/// `b * per_block .. (b + 1) * per_block`, skipping unused ones.
struct Records {
    blocks: Vec<Block>,
    /// Position of each record inside its block, or `NONE`.
    slots: Vec<u32>,
    per_block: u64,
}

const NONE: u32 = u32::MAX;

impl Records {
    fn new(blocks: Vec<Block>, record_count: u64, per_block: u64) -> Self {
        let mut slots = vec![NONE; record_count as usize];
        slots
            .par_chunks_mut(per_block as usize)
            .zip(&blocks)
            .for_each(|(slots, block)| {
                for (position, record) in block.records.iter().enumerate() {
                    slots[(record.number % per_block) as usize] = position as u32;
                }
            });
        Self {
            blocks,
            slots,
            per_block,
        }
    }

    fn get(&self, number: u64) -> Option<&FileRecord> {
        let slot = *self.slots.get(number as usize)?;
        (slot != NONE).then(|| &self.blocks[(number / self.per_block) as usize].records[slot as usize])
    }

    fn get_mut(&mut self, number: u64) -> Option<&mut FileRecord> {
        let slot = *self.slots.get(number as usize)?;
        (slot != NONE).then(|| &mut self.blocks[(number / self.per_block) as usize].records[slot as usize])
    }

    fn name(&self, name: &FileName) -> &[u16] {
        let start = name.start as usize;
        &self.blocks[name.block as usize].names[start..start + usize::from(name.length)]
    }

    fn iter(&self) -> impl ParallelIterator<Item = &FileRecord> {
        self.blocks.par_iter().flat_map_iter(|block| &block.records)
    }

    /// Joins extension records to their base records.
    fn merge_extensions(&mut self) {
        let extensions: Vec<(u64, Vec<FileName>, Option<StandardInformation>, Option<u64>, Option<u64>)> = self
            .iter()
            .filter(|record| record.base != 0)
            .map(|record| {
                (record.base, record.names().copied().collect(), record.times, record.data_size, record.name_size)
            })
            .collect();
        for (base, names, times, data_size, name_size) in extensions {
            let Some(record) = self.get_mut(base & RECORD_MASK) else { continue };
            if record.base != 0 || record.sequence != (base >> 48) as u16 {
                continue;
            }
            for name in names {
                record.add_name(name);
            }
            record.times = record.times.or(times);
            record.data_size = record.data_size.or(data_size);
            record.name_size = record.name_size.or(name_size);
        }
    }

    /// The live base record of a folder named by a file reference. A
    /// stale sequence number means the folder was deleted.
    fn folder(&self, reference: u64) -> Option<&FileRecord> {
        let record = self.get(reference & RECORD_MASK)?;
        let sequence = (reference >> 48) as u16;
        (record.base == 0 && record.is_dir && (sequence == 0 || sequence == record.sequence))
            .then_some(record)
    }
}

/// Builds full paths for every record, in [`crate::search::compare_paths`] order.
fn build_entries(blocks: Vec<Block>, record_count: u64, per_block: u64, root: &Path) -> Result<Vec<Entry>> {
    let mut records = Records::new(blocks, record_count, per_block);
    records.merge_extensions();
    let records = &records;

    // Number the folders so per-folder data can live in dense arrays.
    let mut folder_ids = vec![NONE; record_count as usize];
    let mut folders = Vec::new();
    for block in &records.blocks {
        for record in &block.records {
            if record.base == 0 && record.is_dir {
                folder_ids[record.number as usize] = folders.len() as u32;
                folders.push(record.number);
            }
        }
    }
    let parent_id = |reference: u64| -> Option<u32> {
        records
            .folder(reference)
            .map(|folder| folder_ids[folder.number as usize])
    };
    let paths = folder_paths(records, &folders, parent_id, root.as_os_str());

    // List every reachable name as (parent folder, record, name), grouped by
    // folder and sorted by name inside each folder.
    let mut items: Vec<(u32, u32, u32)> = records
        .iter()
        .filter(|record| record.base == 0 && record.number >= FIRST_USER_RECORD)
        .flat_map_iter(|record| {
            let paths = &paths;
            record.names().enumerate().filter_map(move |(index, name)| {
                let parent = parent_id(name.parent)?;
                paths[parent as usize].as_ref()?;
                Some((parent, record.number as u32, index as u32))
            })
        })
        .collect();
    items.par_sort_unstable_by_key(|&(parent, _, _)| parent);
    let name_of = |&(_, record, index): &(u32, u32, u32)| {
        let record = records.get(u64::from(record)).expect("listed record");
        records.name(record.name(index as usize))
    };
    items
        .par_chunk_by_mut(|left, right| left.0 == right.0)
        .for_each(|group| group.sort_unstable_by(|left, right| compare_names(name_of(left), name_of(right))));

    // A depth-first walk over the sorted groups yields the entries in path
    // order, so later sorts can use the entry index instead of the path.
    let mut children = vec![(0u32, 0u32); folders.len()];
    let mut start = 0;
    for group in items.chunk_by(|left, right| left.0 == right.0) {
        children[group[0].0 as usize] = (start as u32, (start + group.len()) as u32);
        start += group.len();
    }
    // Without a readable root record, the walk gives better results.
    let root = folder_ids
        .get(ROOT_RECORD as usize)
        .copied()
        .filter(|&id| id != NONE)
        .context("the root folder record is damaged")?;
    let mut order = Vec::with_capacity(items.len());
    {
        let mut stack = vec![children[root as usize]];
        while let Some((next, end)) = stack.last_mut() {
            if next == end {
                stack.pop();
                continue;
            }
            let item = items[*next as usize];
            *next += 1;
            order.push(item);
            let (_, record, index) = item;
            let folder = folder_ids[record as usize];
            if index == 0 && folder != NONE {
                stack.push(children[folder as usize]);
            }
        }
    }

    Ok(order
        .into_par_iter()
        .map(|(parent, record, index)| {
            let record = records.get(u64::from(record)).expect("listed record");
            let parent = paths[parent as usize].as_deref().expect("reachable folder");
            make_entry(record, join(parent, records.name(record.name(index as usize))))
        })
        .collect())
}

/// Orders UTF-16 names by code point, which is the byte order of their
/// UTF-8 (or WTF-8) form. Unpaired surrogates keep their own value.
fn compare_names(left: &[u16], right: &[u16]) -> std::cmp::Ordering {
    if left.iter().chain(right).all(|&unit| unit < 0xD800) {
        return left.cmp(right);
    }
    code_points(left).cmp(code_points(right))
}

fn code_points(name: &[u16]) -> impl Iterator<Item = u32> + '_ {
    char::decode_utf16(name.iter().copied()).map(|unit| match unit {
        Ok(ch) => u32::from(ch),
        Err(error) => u32::from(error.unpaired_surrogate()),
    })
}

/// Full path of every folder, by folder id. `None` marks folders that are
/// unreachable from the root, such as the contents of `$Extend`.
fn folder_paths(
    records: &Records,
    folders: &[u64],
    parent_id: impl Fn(u64) -> Option<u32>,
    root: &OsStr,
) -> Vec<Option<OsString>> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        New,
        Visiting,
        Done,
    }
    let mut state = vec![State::New; folders.len()];
    let mut paths: Vec<Option<OsString>> = vec![None; folders.len()];
    let mut stack = Vec::new();
    for start in 0..folders.len() {
        if state[start] == State::Done {
            continue;
        }
        stack.push(start);
        while let Some(&current) = stack.last() {
            if state[current] == State::Done {
                stack.pop();
                continue;
            }
            let record = records.get(folders[current]).expect("folder record");
            let name = record.first_name.as_ref();
            let parent = if record.number == ROOT_RECORD {
                paths[current] = Some(root.to_owned());
                None
            } else if record.number < FIRST_USER_RECORD {
                None
            } else {
                name.and_then(|name| parent_id(name.parent)).map(|id| id as usize)
            };
            match parent.map(|parent| (parent, state[parent])) {
                Some((parent, State::New)) => {
                    state[current] = State::Visiting;
                    stack.push(parent);
                    continue;
                }
                Some((parent, State::Done)) => {
                    paths[current] = paths[parent]
                        .as_ref()
                        .zip(name)
                        .map(|(path, name)| join(path, records.name(name)));
                }
                // A cycle or a missing parent: the folder is unreachable.
                Some((_, State::Visiting)) | None => {}
            }
            state[current] = State::Done;
            stack.pop();
        }
    }
    paths
}

fn make_entry(record: &FileRecord, path: OsString) -> Entry {
    let mut attributes = record.times.map_or(0, |times| times.attributes) & !NON_API_ATTRIBUTES;
    if record.is_dir {
        attributes |= FILE_ATTRIBUTE_DIRECTORY;
    }
    if attributes == 0 {
        attributes = FILE_ATTRIBUTE_NORMAL;
    }
    Entry {
        path: PathBuf::from(path),
        is_dir: record.is_dir,
        size: (!record.is_dir).then(|| record.data_size.or(record.name_size).unwrap_or(0)),
        modified: record.times.map(|times| times.modified),
        created: record.times.map(|times| times.created),
        attributes: Some(attributes),
    }
}

/// `parent` + separator + `name`, built with one exact-size allocation for
/// valid Unicode names.
fn join(parent: &OsStr, name: &[u16]) -> OsString {
    let separator = !matches!(parent.as_encoded_bytes().last(), Some(b'\\' | b'/'));
    if let Some(parent) = parent.to_str() {
        let length: Option<usize> = char::decode_utf16(name.iter().copied())
            .map(|unit| unit.ok().map(char::len_utf8))
            .sum();
        if let Some(length) = length {
            let mut path = String::with_capacity(parent.len() + usize::from(separator) + length);
            path.push_str(parent);
            if separator {
                path.push(std::path::MAIN_SEPARATOR);
            }
            path.extend(char::decode_utf16(name.iter().copied()).map(|unit| unit.unwrap_or('\u{FFFD}')));
            return path.into();
        }
    }
    // Names with unpaired surrogates keep them on Windows.
    let mut path = OsString::from(parent);
    if separator {
        path.push(std::path::MAIN_SEPARATOR_STR);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        path.push(OsString::from_wide(name));
    }
    #[cfg(not(windows))]
    path.push(String::from_utf16_lossy(name));
    path
}

fn round_up(value: u64, multiple: u64) -> u64 {
    value.div_ceil(multiple) * multiple
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_runs_decode_relative_and_sparse_offsets() {
        // 0x10 clusters at LCN 0x1000, then 0x08 sparse, then 0x20 at LCN 0x1000 - 0x10.
        let bytes = [0x21, 0x10, 0x00, 0x10, 0x01, 0x08, 0x21, 0x20, 0xF0, 0xFF, 0x00];
        let runs = parse_runs(&bytes, 0).unwrap();
        assert_eq!(
            runs,
            [
                Run { vcn: 0, lcn: Some(0x1000), clusters: 0x10 },
                Run { vcn: 0x10, lcn: None, clusters: 0x08 },
                Run { vcn: 0x18, lcn: Some(0x0FF0), clusters: 0x20 },
            ]
        );
        assert!(parse_runs(&[0x21, 0x10], 0).is_none(), "truncated run list");
        assert!(parse_runs(&[0x11, 0x01, 0x80, 0x00], 0).is_none(), "negative LCN");
    }

    #[test]
    fn fixups_restore_sector_tails_and_reject_torn_records() {
        let mut record = vec![0u8; 1024];
        record[..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        record[0x30..0x36].copy_from_slice(&[0xAB, 0xCD, 0x11, 0x22, 0x33, 0x44]);
        record[510..512].copy_from_slice(&[0xAB, 0xCD]);
        record[1022..1024].copy_from_slice(&[0xAB, 0xCD]);
        let mut torn = record.clone();
        assert!(apply_fixups(&mut record));
        assert_eq!(record[510..512], [0x11, 0x22]);
        assert_eq!(record[1022..1024], [0x33, 0x44]);

        torn[1022] = 0;
        assert!(!apply_fixups(&mut torn));
        assert!(!apply_fixups(&mut [0u8; 1024]));
    }

    #[test]
    fn boot_sector_geometry() {
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        boot[0x0D] = 8;
        boot[0x30..0x38].copy_from_slice(&786_432u64.to_le_bytes());
        boot[0x40] = 0xF6; // -10: 1024-byte records
        assert_eq!(
            parse_boot_sector(&boot).unwrap(),
            Geometry { cluster_size: 4096, record_size: 1024, mft_cluster: 786_432 }
        );
        boot[0x40] = 1; // one cluster per record
        assert_eq!(parse_boot_sector(&boot).unwrap().record_size, 4096);
        boot[3] = b'X';
        assert!(parse_boot_sector(&boot).is_err());
    }

    #[test]
    fn names_order_like_their_utf8_bytes() {
        let names = ["b", "a", "A", "\u{FF5E}", "\u{1F600}", "ab", "a b", ""];
        let mut by_units: Vec<Vec<u16>> = names.iter().map(|name| name.encode_utf16().collect()).collect();
        by_units.sort_by(|left, right| compare_names(left, right));
        let mut by_bytes = names.to_vec();
        by_bytes.sort();
        let decoded: Vec<String> = by_units.iter().map(|name| String::from_utf16(name).unwrap()).collect();
        assert_eq!(decoded, by_bytes);
    }

    #[test]
    fn drive_roots_are_recognized() {
        assert_eq!(drive_root_letter(Path::new(r"C:\")), Some('C'));
        assert_eq!(drive_root_letter(Path::new(r"\\?\d:\")), Some('D'));
        assert_eq!(drive_root_letter(Path::new(r"C:")), None);
        assert_eq!(drive_root_letter(Path::new(r"C:\Windows")), None);
        assert_eq!(drive_root_letter(Path::new(r"\\server\share\")), None);
    }

    /// Compares the MFT reader with the directory walk on an NTFS image.
    ///
    /// Set `EVERYRUST_NTFS_IMAGE` to the image and `EVERYRUST_NTFS_MOUNT` to a
    /// read-only mount of the same image, then run with `--ignored`.
    #[test]
    #[ignore = "needs an NTFS image"]
    fn image_matches_directory_walk() {
        let image = PathBuf::from(std::env::var_os("EVERYRUST_NTFS_IMAGE").unwrap());
        let mount = PathBuf::from(std::env::var_os("EVERYRUST_NTFS_MOUNT").unwrap());
        let started = std::time::Instant::now();
        let readers = std::env::var("EVERYRUST_MFT_READERS").map_or(8, |value| value.parse().unwrap());
        let from_mft = read_volume(|| std::fs::File::open(&image), readers, &mount).unwrap();
        let mft_time = started.elapsed();
        let started = std::time::Instant::now();
        let from_walk = crate::scan::scan(&mount).unwrap();
        let walk_time = started.elapsed();
        eprintln!("MFT {mft_time:?}, walk {walk_time:?}, {} entries", from_mft.len());

        assert!(
            from_mft
                .windows(2)
                .all(|pair| crate::search::compare_paths(&pair[0].path, &pair[1].path).is_lt()),
            "MFT entries are not in path order"
        );
        let key = |entry: &Entry| (entry.path.clone(), entry.is_dir, entry.size, entry.modified);
        let mut mft: Vec<_> = from_mft.iter().map(key).collect();
        let mut walk: Vec<_> = from_walk.iter().map(key).collect();
        mft.sort();
        walk.sort();
        let only_mft: Vec<_> = mft.iter().filter(|item| walk.binary_search(item).is_err()).take(10).collect();
        let only_walk: Vec<_> = walk.iter().filter(|item| mft.binary_search(item).is_err()).take(10).collect();
        assert!(only_mft.is_empty() && only_walk.is_empty(), "only MFT: {only_mft:#?}\nonly walk: {only_walk:#?}");
    }
}
