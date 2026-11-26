# Changelog

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
