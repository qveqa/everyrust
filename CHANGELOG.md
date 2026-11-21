# Changelog

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
