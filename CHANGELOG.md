# Changelog / История изменений

All notable changes are documented here. / Все заметные изменения описаны здесь.

## [1.1.0] — 2026-11-16

<!-- lang:en -->
### Charts, areas, styles, sample, report, command line
- **Charts and SmartArt** (Word, Excel, PowerPoint) are changed too; they were skipped silently before.
- **Left over:** after a real run the files are read again, and anything the rules would still change is marked as left over, with a hint when the text sits in an embedded object or a picture.
- **Areas:** a rule can be limited to the body, headers, footers, footnotes, endnotes, comments or style definitions (the "Apply to" picker, or `scope` in a template).
- **Paragraph styles:** a rule can be limited to one Word paragraph style (`style`); `Normal` also reaches paragraphs without an explicit style.
- **Priority:** the most specific rule wins: paragraph style, then area, then everywhere.
- **From a sample…** takes the most used font and size from a document; "Save report…" writes the results as CSV (UTF-8 with a BOM).
- **Check fonts…** lists the fonts the documents use but this computer does not have.
- **`.fctpl` version 2** (rules may carry `scope` and `style`); version 1 files still load.
- **Command line** without a window: `--folder`, `--preset`, `--report`, `--mask`, `--dry`, `--no-backup`, `--flat`, `--help`; exit code 0/1/2. Only known arguments start it, anything else opens the window.

<!-- lang:ru -->
### Диаграммы, области, стили, образец, отчёт, командная строка
- **Диаграммы и SmartArt** (Word, Excel, PowerPoint) теперь тоже меняются; раньше они молча пропускались.
- **Осталось:** после реального прогона файлы читаются заново, и всё, что правила всё ещё изменили бы, помечается как «осталось»; если текст в встроенном объекте или картинке, есть подсказка.
- **Области:** правило можно ограничить основным текстом, колонтитулами, сносками, концевыми сносками, примечаниями или определениями стилей (выбор «Применить к» или `scope` в шаблоне).
- **Стили абзаца:** правило можно ограничить одним стилем абзаца Word (`style`); `Normal` затрагивает и абзацы без явного стиля.
- **Приоритет:** побеждает самое точное правило: стиль абзаца, затем область, затем «везде».
- **«По образцу…»** берёт самые частые шрифт и размер из документа; «Сохранить отчёт…» записывает результаты в CSV (UTF-8 с BOM).
- **«Проверить шрифты…»** показывает шрифты, которые есть в документах, но не установлены на этом компьютере.
- **`.fctpl` версии 2** (в правилах могут быть `scope` и `style`); файлы версии 1 загружаются.
- **Командная строка** без окна: `--folder`, `--preset`, `--report`, `--mask`, `--dry`, `--no-backup`, `--flat`, `--help`; код возврата 0/1/2. Запускается только по известным аргументам, любые другие открывают окно.

## [1.0.0] — 2026-11-14

<!-- lang:en -->
### First release
- Changes fonts and font sizes in Word, Excel, PowerPoint, LibreOffice, RTF, HTML and CSS files.
- Rules: font A → font B, any font → font B, size X → size Y, any size → size Y, in any combination; several rules in a list or loaded from a file.
- Preview, `.bak` backups, atomic writes; symbol fonts and Asian / complex-script fonts are protected from "any font" rules unless you choose otherwise.
- Scanning: the fonts and sizes found in the selected files are suggested in the "Find" fields (grey hint for a single font, drop-down list for several); automatic scanning can be switched off.
- Rule templates: "Save rules…" writes a `.fctpl` file (JSON, versioned) with the rules, the mask and the options; "Load rules…" reads `.fctpl` and old tab-separated files.
- The size list says where each size comes from (in text / in styles / hidden); the "found" hint counts only sizes of visible text; common fonts and sizes are always offered in the lists.
- "Any size" leaves sizes below a limit (6 pt by default) alone, so thin spacer lines keep their height.
- Language, theme and the update check moved to a gear pop-over in the top bar.
- Linux: the program is installed under "Office" (.deb template and an AppImage menu installer script).
- The columns of the results table can be resized by dragging their borders (double click = fit to content); the widths are remembered. A cell's full text is also shown as a tooltip.

<!-- lang:ru -->
### Первый выпуск
- Меняет шрифты и размеры шрифтов в файлах Word, Excel, PowerPoint, LibreOffice, RTF, HTML и CSS.
- Правила: шрифт A → шрифт B, любой шрифт → шрифт B, размер X → размер Y, любой размер → размер Y, в любых сочетаниях; несколько правил в списке или из файла.
- Предпросмотр, резервные копии `.bak`, атомарная запись; символьные шрифты и шрифты азиатских и сложных письменностей защищены от правил «любой шрифт», если не выбрано иное.
- Сканирование: найденные в выбранных файлах шрифты и размеры подсказываются в полях «Найти» (серая подсказка для одного шрифта, выпадающий список для нескольких); автосканирование можно отключить.
- Шаблоны правил: «Сохранить правила…» записывает файл `.fctpl` (JSON с версией формата) с правилами, маской и опциями; «Загрузить правила…» читает `.fctpl` и старые файлы с табуляциями.
- Список размеров показывает, откуда размер (в тексте / в стилях / скрытый); подсказка «найден» учитывает только размеры видимого текста; типовые шрифты и размеры всегда есть в списках.
- «Любой размер» не трогает размеры меньше порога (по умолчанию 6 пт), поэтому тонкие строки-разделители сохраняют высоту.
- Язык, тема и проверка обновлений перенесены во всплывающее окно с шестерёнкой в шапке.
- Linux: программа ставится в раздел «Офис» (шаблон для .deb и скрипт добавления AppImage в меню).
- Ширину столбцов таблицы результатов можно менять, перетаскивая границы (двойной щелчок — по ширине содержимого); ширина запоминается. Полный текст ячейки виден и во всплывающей подсказке.
