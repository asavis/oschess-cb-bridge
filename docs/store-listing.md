# Microsoft Store listing

The texts for the bridge's Partner Center submission (#112,
[release.md](release.md#microsoft-store)), Ukrainian first. They describe the
Store's copy, which updates itself through the Store (#153).

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
> партій, перегляд і аналіз. Бази ChessBase міст лише читає; файл PGN
> змінюється, лише коли ви зберігаєте в нього партію в oschess.
> • Рушій для аналізу: міст знаходить рушії ChessBase і Fritz або однією
> кнопкою встановлює офіційну збірку Stockfish з GitHub. Аналітична дошка
> oschess рахує на вашому процесорі.
> • Міст працює у фоні, знак біля годинника, і за бажанням стартує разом з
> Windows.
>
> Міст відповідає лише браузеру, який ви з ним з’єднали, і лише на адресі
> цього комп’ютера. В інтернет він звертається лише в трьох випадках. Через
> хвилину після запуску й далі кожні шість годин він питає Microsoft Store,
> чи вийшла нова версія, і оновлюється через Store; «Оновлювати автоматично»
> в налаштуваннях вимикає ці перевірки. Коли ви просите встановити
> Stockfish, міст завантажує його офіційну збірку з GitHub. А Stockfish, який
> він установив, міст тримає свіжим: у ті самі години питає GitHub, чи вийшов
> новий Stockfish, і встановлює його через тиждень після виходу; «Оновлювати
> Stockfish автоматично» в налаштуваннях вимикає це.
>
> Відкритий код, ліцензія AGPL-3.0: https://github.com/asavis/oschess-cb-bridge

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
> browse and analyse games. The bridge only reads ChessBase databases; a PGN
> file changes only when you save a game into it in oschess.
> • An engine for analysis: the bridge finds the engines of ChessBase and
> Fritz, or installs the official Stockfish build from GitHub with one button.
> The oschess analysis board calculates on your own processor.
> • The bridge runs in the background, as a mark next to the clock, and can
> start with Windows.
>
> The bridge answers only the browser you paired it with, and only on this
> computer's own address. It goes online in three cases only. A minute after
> it starts, and every six hours after that, it asks the Microsoft Store
> whether a new version is out, and updates through the Store; «Update
> automatically» in the settings turns these checks off. When you ask it to
> install Stockfish, it downloads the official build from GitHub. And it keeps
> the Stockfish it installed up to date: at the same times it asks GitHub
> whether a new Stockfish is out, and installs it once it is a week old;
> «Update Stockfish automatically» in the settings turns this off.
>
> Open source under the AGPL-3.0 licence: https://github.com/asavis/oschess-cb-bridge

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
> in the notification area. It reads ChessBase databases from folders the user
> picks, serves them on 127.0.0.1 to the oschess web app the user paired it
> with, writes only PGN files the user saves games to, and runs the UCI engine
> the user chose as a child process: one from ChessBase or Fritz, or the
> official Stockfish it downloads from GitHub and checks by SHA-256. This
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
downloads the official Stockfish build from Stockfish's GitHub releases when
the user asks, and keeps a build it installed up to date while «Update
Stockfish automatically» is on (the default, which the first-run wizard
shows), a release only once it is a week old, checking every download against
the SHA-256 GitHub publishes for it (#322).

Submit only once the ChessBase section is open to everyone on oschess.org
(asavis/oschess#13368): until then the pairing link shows the testers no
ChessBase section, and certification can fail the app for not doing what the
listing says.
