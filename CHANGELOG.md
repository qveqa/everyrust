# Changelog

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
