# Changelog

## 0.2.8

### Русский

- Индекс хранится как дерево имён, как в Everything: вместо полного пути у каждой записи есть имя, ссылка на папку и метаданные в общих массивах. На 878 тыс. файлов индекс занимает около 90 МБ вместо 470 МБ.
- MFT сразу строит дерево, без промежуточных полных путей.
- Журнал изменений NTFS (USN): после первого чтения MFT приложение сохраняет позицию в журнале. При следующем запуске оно применяет только изменения с прошлого раза, без повторного чтения MFT. Во время работы изменения тоже приходят из журнала, поэтому массовые изменения файлов больше не вызывают полного пересканирования.
- Индекс сохраняется и при закрытии окна. Формат файла индекса изменился; старый файл будет создан заново.
- Из строки состояния убраны время индексации, способ и подсказка с подробностями.

### English

- The index is a tree of names, as in Everything: instead of a full path, each entry has a name, a link to its folder, and metadata in shared arrays. For 878,000 files the index takes about 90 MB instead of 470 MB.
- The MFT reader builds the tree directly, without full paths in between.
- NTFS change journal (USN): after the first MFT read, the app saves its position in the journal. On the next start, it applies only the changes since the last run, without reading the MFT again. While the app runs, changes also come from the journal, so mass file changes no longer cause a full rescan.
- The index is also saved when the window closes. The index file format changed; the old file is created again.
- The status bar no longer shows the indexing time, the method, or the tooltip with details.

## 0.2.7

### Русский

- После сканирования индекс сохраняется в `%LOCALAPPDATA%\EveryRust`. При следующем запуске окно показывает сохранённый индекс сразу (1,5 млн записей загружаются примерно за 0,1 с), а свежий индекс заменяет его в фоне. `EVERYRUST_NO_CACHE` отключает сохранение.
- Строка состояния показывает способ индексации и время, например `indexed in 0.42 s (MFT)`. Если MFT прочитать не удалось, подсказка показывает причину.
- Чтение MFT пропускает блоки без используемых записей по битовой карте `$MFT`.
- На жёстком диске MFT читается одним потоком по порядку, на SSD — четырьмя потоками. Тип диска определяется через `IOCTL_STORAGE_QUERY_PROPERTY`. На NVMe четыре потока оказались быстрее всего, а 16 потоков (0.2.6 открывал том заново на каждый блок) — медленнее одного.
- Том читается мимо кэша Windows (`FILE_FLAG_NO_BUFFERING`) в буферы, выровненные по 4 КиБ. Том открывается один раз на поток чтения, а не на каждый блок.
- Подсказка над строкой состояния показывает, на что ушло время чтения MFT: размер, открытие, чтение со скоростью и построение путей. `EVERYRUST_DEBUG_SCAN` выводит то же в консоль.
- Настройки чтения MFT для замеров: `EVERYRUST_MFT_READERS`, `EVERYRUST_MFT_BLOCK_KIB`, `EVERYRUST_MFT_BUFFERED`.

### English

- After a scan, the index is saved to `%LOCALAPPDATA%\EveryRust`. On the next start, the window shows the saved index at once (1.5 million entries load in about 0.1 s), and the fresh index replaces it in the background. `EVERYRUST_NO_CACHE` turns off saving.
- The status bar shows how the index was built and how long it took, for example `indexed in 0.42 s (MFT)`. When the MFT cannot be read, a tooltip shows why.
- MFT reading skips blocks without records in use, according to the `$MFT` bitmap.
- On a hard disk, the MFT is read by one thread in disk order; on an SSD, by four threads. The disk type comes from `IOCTL_STORAGE_QUERY_PROPERTY`. On an NVMe drive, four threads were fastest, and 16 threads were slower than one.
- The volume is read without the Windows file cache (`FILE_FLAG_NO_BUFFERING`) into buffers aligned to 4 KiB. Each reader thread opens the volume once instead of once per block (0.2.6 opened it again for every block).
- The status bar tooltip shows where the MFT read time went: size, open, read with throughput, and path building. `EVERYRUST_DEBUG_SCAN` prints the same to the console.
- MFT read settings for measurements: `EVERYRUST_MFT_READERS`, `EVERYRUST_MFT_BLOCK_KIB`, `EVERYRUST_MFT_BUFFERED`.

## 0.2.6

### Русский

- Корень диска NTFS (по умолчанию `C:\`) индексируется чтением главной таблицы файлов (MFT) напрямую с тома, как в Everything. Блоки MFT читаются и разбираются параллельно. Обход каталогов остаётся запасным вариантом без прав администратора и для дисков не NTFS. `EVERYRUST_NO_MFT` отключает чтение MFT.
- На образе NTFS со 141 тыс. записей чтение MFT заняло около 70 мс, а обход каталогов того же образа — от 4,5 до 7,3 с. Замер сделан в Wine, а не в Windows; на реальном диске время зависит от скорости чтения MFT.
- Обход каталогов использует `FindFirstFileExW` с `FIND_FIRST_EX_LARGE_FETCH`: больше записей за один системный вызов.
- Записи из MFT идут в порядке путей, поэтому сортировка по пути почти мгновенна, а другие сортировки сравнивают номера записей вместо путей. Сравнение путей работает с байтами. На 1,5 млн записей начальная сортировка по размеру ускорилась с 411 до 37 мс, построение порядка путей — с 1070 до 4 мс.
- Распределитель памяти mimalloc ускоряет построение индекса и освобождение памяти. Старый индекс после `F5` освобождается в фоновом потоке, поэтому окно не замирает.

### English

- An NTFS drive root (`C:\` by default) is indexed by reading the Master File Table (MFT) directly from the volume, as Everything does. MFT blocks are read and parsed in parallel. The directory walk stays as the fallback without administrator privileges and for drives that are not NTFS. `EVERYRUST_NO_MFT` turns off MFT reading.
- On an NTFS image with 141,000 entries, reading the MFT took about 70 ms, and walking the directories of the same image took 4.5 to 7.3 s. This was measured in Wine, not Windows; on a real drive the time depends on how fast the MFT can be read.
- The directory walk uses `FindFirstFileExW` with `FIND_FIRST_EX_LARGE_FETCH`, which returns more entries per system call.
- MFT entries arrive in path order, so sorting by path is almost instant and other sorts compare entry numbers instead of paths. Path comparison works on bytes. With 1.5 million entries, the initial size sort dropped from 411 ms to 37 ms, and building the path order from 1,070 ms to 4 ms.
- The mimalloc allocator speeds up index building and freeing memory. After `F5`, the old index is freed on a background thread, so the window does not freeze.

## 0.2.5

### Русский

- Двойной клик и `Enter` открывают файл через Windows Shell, а не через `cmd`. Раньше имя файла с `&`, например `invoice&calc.exe`, могло запустить другую команду, в том числе с правами администратора.
- По умолчанию приложение индексирует только диск `C:\`. Другие диски можно открыть через **Open Folder** или аргумент командной строки. При переполнении очереди событий пересканируется только `C:\`, а не все диски.
- **Restart as Administrator** правильно передаёт пути с пробелами, кавычками и `\` на конце, например `"D:\My Work\"`.
- `Ctrl+C` копирует путь выбранного файла и учитывает событие копирования egui. Если в поле поиска выделен текст, копируется этот текст, а не путь.
- **Export Results** показывает ошибку записи в строке состояния.
- Даты файлов до 1970 года сохраняются. Даты ISO 8601 до 1601 года отклоняются, а не превращаются в неверное значение.
- В README убрана ссылка на `dist/everyrust.exe`, которого нет в репозитории. Замер скорости обновлён по данным 0.2.0, описаны `Ctrl+O`, `Enter` и `Esc`.

### English

- Double-click and `Enter` open files through the Windows Shell instead of `cmd`. Before, a file name with `&`, such as `invoice&calc.exe`, could run another command, also with administrator privileges.
- By default, the app indexes only drive `C:\`. Open other drives with **Open Folder** or a command-line argument. When the event queue overflows, only `C:\` is rescanned instead of all drives.
- **Restart as Administrator** passes paths with spaces, quotes, and a trailing `\` correctly, for example `"D:\My Work\"`.
- `Ctrl+C` copies the selected item's path and handles the egui copy event. When text is selected in the search box, `Ctrl+C` copies that text instead of the path.
- **Export Results** shows write errors in the status bar.
- File dates before 1970 are kept. ISO 8601 dates before 1601 are rejected instead of becoming a wrong value.
- The README no longer refers to `dist/everyrust.exe`, which is not in the repository. The benchmark now uses the 0.2.0 measurement, and `Ctrl+O`, `Enter`, and `Esc` are documented.

## 0.2.0

### Русский

- `Del` отправляет выделенный файл в Корзину Windows и сразу убирает его из результатов.
- Мониторинг обновляет индекс файлов любого размера. Снят лимит в 500 МиБ.
- Ускорен первый обход и повторное сканирование. Подпапки и локальные диски обрабатываются через общую очередь. На `C:\Windows` с 212 тыс. записей и прогретым кэшем медианное время сканирования, построения индекса и начальной сортировки составило 387 мс вместо 471 мс (на 18% меньше).
- **Open Folder** открывает Проводник с выделенным файлом, в том числе для путей с пробелами, кириллицей и запятыми.

### English

- Press `Del` to move the selected file to the Windows Recycle Bin and remove it from the results immediately.
- Live indexing tracks files of any size. The 500 MiB limit has been removed.
- Initial scans and rescans use a shared queue for subfolders and local drives. On `C:\Windows` with 212,000 entries and a warm cache, the median time to scan, index, and sort the initial results dropped from 471 ms to 387 ms (18%).
- **Open Folder** selects the file in Explorer, including paths with spaces, Cyrillic characters, and commas.

## 0.1.0

Первый публичный выпуск / First public release. [GitHub release](https://github.com/qveqa/everyrust/releases/tag/v0.1.0)
