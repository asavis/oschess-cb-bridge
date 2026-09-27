# Microsoft Store listing

The texts for the bridge's Partner Center submission (#112,
[release.md](release.md#microsoft-store)), Ukrainian first. They describe the
Store's copy, which carries Stockfish and is updated by the Store.

## Properties

- **Category:** Utilities & tools.
- **Privacy policy:** https://github.com/asavis/oschess-cb-bridge#code-signing-policy
- **Website:** https://github.com/asavis/oschess-cb-bridge
- **Support contact:** https://github.com/asavis/oschess-cb-bridge/issues
- **Pricing:** free, in every market.
- **System requirements:** Windows 10 version 1809 or later, x64; Windows 11
  recommended.

## Listing (uk-UA)

**Назва:** oschess bridge

**Короткий опис:**

> Відкривайте свої бази ChessBase і шаховий рушій в oschess.

**Опис:**

> oschess bridge — невелика програма для Windows, яка з’єднує ваш комп’ютер з
> oschess (https://oschess.org).
>
> • Бази ChessBase з вашого диска відкриваються в бібліотеці oschess: пошук
> партій, перегляд і аналіз. Міст лише читає бази й нічого в них не змінює.
> • Офіційна збірка Stockfish входить до мосту: аналітична дошка oschess
> рахує на вашому процесорі. Можна обрати й інший UCI-рушій.
> • Міст працює у фоні, знак біля годинника, і за бажанням стартує разом з
> Windows.
>
> Міст відповідає лише браузеру, який ви з ним з’єднали, і лише на адресі
> цього комп’ютера. Сам він нічого не надсилає в інтернет.
>
> Відкритий код, ліцензія MIT: https://github.com/asavis/oschess-cb-bridge

**Можливості:**

- Бази ChessBase в oschess, без завантаження на сервер
- Stockfish для аналізу на вашому комп’ютері
- Робота у фоні й запуск з Windows
- Українська та англійська мови

## Listing (en-US)

**Name:** oschess bridge

**Short description:**

> Open your ChessBase databases and a chess engine in oschess.

**Description:**

> oschess bridge is a small Windows app that connects your computer to oschess
> (https://oschess.org).
>
> • ChessBase databases on your disk open in the oschess Library: search,
> browse and analyse games. The bridge only reads the databases and never
> changes them.
> • The official Stockfish build comes with the bridge, so the oschess analysis
> board calculates on your own processor. You can choose another UCI engine.
> • The bridge runs in the background, as a mark next to the clock, and can
> start with Windows.
>
> The bridge answers only the browser you paired it with, and only on this
> computer's own address. It sends nothing to the internet itself.
>
> Open source under the MIT licence: https://github.com/asavis/oschess-cb-bridge

**Features:**

- ChessBase databases in oschess, never uploaded
- Stockfish analysis on your own computer
- Runs in the background and starts with Windows
- Ukrainian and English

## Screenshots

At least one, 1920×1080: oschess in the browser with a ChessBase database
open, and the bridge's settings window with its engine section.

## Age rating (IARC questionnaire)

A utility with no violence, no user-generated content shared with others, no
chat, no purchases, no location and no personal data collected: every
content question is answered «No».

## Restricted capability: runFullTrust

> oschess bridge is a packaged Win32 desktop app (Windows.FullTrustApplication)
> that lives in the notification area. It reads ChessBase database files from
> any folder the user picks, serves them read-only on a loopback port
> (127.0.0.1) to the oschess web app the user paired it with, and runs a UCI
> chess engine as a child process: the Stockfish build in the package, or an
> engine the user picks. These need the full-trust desktop access that
> runFullTrust grants.

## Notes for certification

> The tray icon's menu opens the settings. The engine section lists the
> Stockfish build the package carries, already chosen. «Open oschess» pairs
> the default browser with the bridge through https://oschess.org and shows the
> Library's ChessBase section, which lists the databases added in the
> settings. Any ChessBase database (.cbh with its companion files) works.

Submit only once the ChessBase section is open to everyone on oschess.org
(asavis/oschess#13368): until then the pairing link shows the testers no
ChessBase section, and certification can fail the app for not doing what the
listing says.
