# Microsoft Store listing

The texts for the bridge's Partner Center submission (#112,
[release.md](release.md#microsoft-store)), Ukrainian first. They describe the
Store's copy, which the Store updates.

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
> • Рушій для аналізу: міст знаходить рушії ChessBase і Fritz або однією
> кнопкою встановлює офіційну збірку Stockfish з GitHub. Аналітична дошка
> oschess рахує на вашому процесорі.
> • Міст працює у фоні, знак біля годинника, і за бажанням стартує разом з
> Windows.
>
> Міст відповідає лише браузеру, який ви з ним з’єднали, і лише на адресі
> цього комп’ютера. В інтернет він звертається лише тоді, коли ви просите
> встановити Stockfish.
>
> Відкритий код, ліцензія MIT: https://github.com/asavis/oschess-cb-bridge

**Можливості:**

- Бази ChessBase в oschess, без завантаження на сервер
- Аналіз рушієм на вашому комп’ютері: ChessBase, Fritz або Stockfish
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
> • An engine for analysis: the bridge finds the engines of ChessBase and
> Fritz, or installs the official Stockfish build from GitHub with one button.
> The oschess analysis board calculates on your own processor.
> • The bridge runs in the background, as a mark next to the clock, and can
> start with Windows.
>
> The bridge answers only the browser you paired it with, and only on this
> computer's own address. It goes online only when you ask it to install
> Stockfish.
>
> Open source under the MIT licence: https://github.com/asavis/oschess-cb-bridge

**Features:**

- ChessBase databases in oschess, never uploaded
- Engine analysis on your own computer: ChessBase, Fritz or Stockfish
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

Partner Center takes at most 500 characters here.

> oschess bridge is a packaged Win32 desktop app (Windows.FullTrustApplication)
> in the notification area. It reads ChessBase database files from folders the
> user picks, serves them read-only on a loopback port (127.0.0.1) to the
> oschess web app the user paired it with, and runs the UCI engine the user
> chose as a child process: one from ChessBase or Fritz, or the official
> Stockfish it downloads on request and checks against a pinned SHA-256. This
> needs full-trust desktop access.

## Notes for certification

The oschess Library needs an account. The testers get the same demo account
as Google Play and App Store review, with its admin-set permanent code
(`docs/mobile-android.md` in the oschess repository, "Store review access").
The owner enters its email and code under Credentials on the Additional Testing
Information page, never in these notes or in this repository.

> Sign-in: the oschess Library needs an account. Use the demo account in the
> Credentials below: at https://oschess.org enter the demo email, select
> Continue, then enter the six-digit code. No login email is sent.
>
> The tray icon's menu opens the settings. In the engine section, «Install
> Stockfish» downloads the official build and chooses it. «Open oschess» pairs
> the default browser with the bridge through https://oschess.org and shows the
> Library's ChessBase section, which lists the databases added in the
> settings. Any ChessBase database (.cbh with its companion files) works.

Certification may ask about an app that downloads an executable. The
listing, the justification above and these notes all say that the bridge
downloads the official Stockfish build only on the user's request and checks
it against a SHA-256 pinned in the release.

Submit only once the ChessBase section is open to everyone on oschess.org
(asavis/oschess#13368): until then the pairing link shows the testers no
ChessBase section, and certification can fail the app for not doing what the
listing says.
