# BitSpring in Google Play

What goes into Play Console for the Android app. English (United States) is the default
language; the Russian listing follows.

The Play build is `npm run build:play` (see "Building" below). It differs from the APK on the
website: package `app.bitspring.player` instead of `dev.local.hifi` (a separate app that can be
installed next to it, the same id as the App Store and Microsoft Store builds), no Qobuz/TIDAL,
no self-update (Play updates it), and no photo permission (so cover.jpg files in the shared
Music folder are not read; embedded art and covers in the app's own folder are). Don't mention
streaming anywhere in the listing.

## Play Console steps

A personal developer account made after November 2023 has to run a **closed test with at least
12 testers for at least 14 days in a row** before it may publish to everyone. Internal testing
doesn't count towards it. In order:

1. **Account.** https://play.google.com/console/signup → Personal account, $25 once, identity
   check with an ID document. Play then asks to verify the account on a real Android device
   with the Play Console app (Google's own check; an emulator won't do). The developer name and
   the contact email are shown on the store page.
2. **Create app.** Home → Create app: name "BitSpring: Bit-Perfect Player", default language
   English (United States), App, Free, accept the declarations.
3. **Set up your app** (Dashboard): fill every item of "App content" and "Store listing" with
   the answers below. Graphics go in Grow users → Store presence → Main store listing.
4. **Closed test track.** Test and release → Testing → Closed testing → the default track
   ("Closed testing - Alpha") → Manage track:
   - **Countries/regions**: add all.
   - **Testers**: pick "Email lists" and make a list (up to 2,000 Gmail or Google Workspace
     addresses, typed or a CSV), or "Google Groups" and enter a group's address
     (e.g. bitspring-testers@googlegroups.com; anyone who joins the group becomes a tester,
     which is easier to share on a forum). Fill "Feedback URL or email address" with
     https://github.com/crash2bash/bitspring/issues. Save.
   - **Create new release**: on the first release Play asks about app signing: keep "Google
     generates and protects the app signing key" (Play App Signing). Upload the signed .aab
     (`store/play/BitSpring-<version>-play.aab`); the first upload makes its signing
     certificate the upload key, so every later bundle must be signed with the same
     `~/.android/hifi-release.jks`. Release name: the version (e.g. 0.23.0); release notes
     below. Next → Save → "Send for review" on the Publishing overview page. The first review
     takes from a few hours to a few days.
5. **Testers join.** After review the track shows "Available to testers" and, under Testers,
   a **"Join on the web" link** (https://play.google.com/apps/testing/app.bitspring.player).
   Each tester opens it while signed in to the Google account on the list (or in the group),
   presses "Become a tester", then installs BitSpring from Play on their phone (the store page
   is found only through that link until production). They must stay opted in: the 14 days
   count only while at least 12 opted-in testers have the app. Ask them to open it now and
   then; Google asks about their engagement later.
6. **Updates during the test.** Each new bundle needs a higher versionCode, i.e. a new app
   version (0.23.0 → 23000). Upload it as a new release on the same closed track.
7. **Apply for production.** When the Dashboard shows that 12 testers have been opted in for
   14 days, "Apply for production" opens. Google asks: how testers were recruited, how easy it
   was to get them, what they said and what was changed after it, who the app is for, how it
   is different, how many installs to expect. Answer plainly. The answer comes in about 7 days
   (sometimes longer); if refused, Google says why and the test goes on.
8. **Production.** Test and release → Production → Create new release → "Add from library"
   (the bundle already tested), countries: all, then Send for review. Optional:
   staged rollout (start at 20%).

Every upload can also take two optional files that Play otherwise warns about, both made by
`npm run build:play`: the R8 mapping (`src-tauri/gen/android/app/build/outputs/mapping/universalRelease/mapping.txt`,
App bundle explorer → Downloads → ReTrace mapping file) and the native debug symbols. They only
make crash stack traces in Play Console readable; skipping them changes nothing for users.

## App details

| Field | Value |
|---|---|
| App name (30 max) | BitSpring: Bit-Perfect Player |
| Package name | app.bitspring.player (permanent) |
| Default language | English (United States) – en-US |
| App or game | App |
| Free or paid | Free (permanent: a free app can never become paid) |
| Category | Music & Audio |
| Tags | Music player, Audio, Equalizer (pick the closest ones offered) |
| Contact email | the developer account's public email (shown on the store page) |
| Website | https://bitspring.app/ |
| Privacy policy | https://bitspring.app/privacy/ |

## Short description (80 max)

```
Bit-perfect hi-res player for FLAC, DSD and USB DACs, with headphone EQ.
```

## Full description (4000 max)

```
BitSpring plays your music exactly as it was recorded. Every bit of the file reaches your USB DAC unchanged: no resampling, no volume tricks, no surprises.

It's free and open source. No account, no subscription, no ads, no in-app purchases, and nothing about you is collected.

BIT-PERFECT
• On Android 14 and later, a USB DAC gets each track at its own sample rate, bit for bit, past the Android mixer.
• A clear indicator shows whether the signal is bit-perfect or being processed, and why.
• Gapless playback for live albums, operas and DJ mixes.

EVERY FORMAT YOU OWN
• FLAC, ALAC, WAV, AIFF, CAF, MKA, APE, WavPack, TTA, MP3, AAC, Ogg Vorbis and Opus.
• DSD (DSF, DFF and SACD disc images): native over DoP on DACs that support it, or converted to PCM.
• Albums ripped to one file with a CUE sheet play track by track.
• Album art, tags and lyrics: from .lrc files and the tags, or looked up on LRCLIB if you turn it on.

KNOW WHAT YOU HAVE
• Quality check: finds "hi-res" files that are upsampled CDs or were made from MP3.
• Dynamic range of every track and album: DR and the EBU R128 loudness range.
• Rip check: compares a CD rip with other people's rips of the same disc in the CUETools Database.
• Blind test (ABX): hear for yourself whether the equalizer or crossfeed changes anything.

ENHANCERS, ONLY WHEN YOU WANT THEM
• Parametric EQ with ready-made corrections for thousands of headphones and IEMs from AutoEQ: pick your model by name.
• Crossfeed for a more natural stereo image on headphones.
• Upsampling with a choice of filters: standard, long linear phase, long minimum phase and apodizing.
• DSD output: any music turned into DSD64 and sent to the DAC over DoP.
• Volume levelling by track or album (ReplayGain tags, or measured with EBU R128), and automatic headroom against clipping.
Turn them all off and the signal is bit-perfect again.

YOUR LIBRARY
• The Music folder, plus any folders you add: in the phone's storage, on a memory card or on a USB drive. Skip the folders you don't want.
• Favourites, star ratings, smart playlists, shuffle and repeat.
• Playlists, saved as M3U, PLS, XSPF or CSV.
• Keeps playing with the screen off, with controls in the notification and on the lock screen.

In 24 languages.
```

## Graphics

| Asset | Size | File |
|---|---|---|
| App icon | 512 × 512 PNG, 32-bit | store/play/icon-512.png |
| Feature graphic | 1024 × 500 PNG or JPEG, no transparency | store/play/feature-1024x500.png |
| Phone screenshots | 2 to 8, 1080 × 1920 (9:16) | store/play/captioned/*.png |

Tablet screenshots are optional; without them the app is still offered on tablets, just
lower in tablet search. (The iPhone screenshots can't be reused: their captions talk about the
iPhone and their status bar is the iPhone's.)

Play takes no screenshot whose long side is more than twice the short one, so the raw
1080 × 2400 phone pictures (`play/phone/`) can't go up as they are: upload the captioned
1080 × 1920 ones in `play/captioned/`, in their order:

1. Every bit reaches your DAC — Bit-perfect over USB on Android 14 and later.
2. See what happens to the sound — From the file to your ears, and what changes it on the way.
3. Your music, your folders — The Music folder, a memory card or a USB drive.
4. EQ made for your headphones — AutoEQ corrections for thousands of models.
5. Enhancers when you want them — Upsampling filters, DSD output, crossfeed, levelling.

How they were made: the feature graphic is `play/feature.html` rendered by headless Chrome
(`--window-size=1024,500 --screenshot`). The phone pictures are the Play build itself on the
Android emulator (AVD hifi35, 1080 × 2400) with the demo albums in /sdcard/Music, the app's
language set to English (`cmd locale set-app-locales app.bitspring.player --locales en-US`) and
a clean status bar (System UI demo mode, clock 12:00). The emulator has no USB DAC, so the
player shows the Android mixer's path, not bit-perfect. Each captioned one is
`play/frame.html?title=…&sub=…&shot=phone/<name>.png` rendered by headless Chrome
(`--allow-file-access-from-files --window-size=1080,1920 --screenshot`).

## Release notes (500 max)

First release:

```
First release in Google Play.
```

## App content (Policy → App content)

Answers for each declaration Play Console asks for:

- **Privacy policy**: https://bitspring.app/privacy/
- **Ads**: No, the app contains no ads.
- **App access**: All functionality is available without special access (no login).
- **Content rating** (IARC questionnaire): category "All other app types"; answer No to every
  question (no violence, sexuality, language, drugs, gambling, user-generated content, sharing
  of location, or purchases). Expected rating: Everyone / PEGI 3 / USK 0.
- **Target audience**: 18 and over (choosing ages under 13 brings in the Families policy and
  extra review; the app isn't made for children).
- **News app**: No.
- **Advertising ID**: No, the app doesn't use it (no ads SDK, no AD_ID permission).
- **Government app**: No. **Financial features**: None. **Health**: None.
- **Data safety**: see below.
- **Foreground service** (Android 14+ declaration, under "Foreground service permissions"):
  type *Media playback*, task "Media playback". Description: "BitSpring is a music player. The
  media playback foreground service keeps the user's music playing with the screen off or the
  app in the background, and shows the playing track with playback controls in the media
  notification and on the lock screen. It starts only when the user presses Play and stops when
  playback stops." Play asks for a video link showing it: a short screen recording (start a
  track, go to the home screen, lock the phone, the notification and lock screen controls
  work) uploaded to YouTube as unlisted.
- **Photo and video permissions**: not asked; the Play build doesn't request
  READ_MEDIA_IMAGES.

### Data safety

- **Does your app collect or share any of the required user data types?** No.
- **Is all of the user data collected by your app encrypted in transit?** Play hides this
  question once the answer above is No; if asked: Yes (every connection is HTTPS).
- **Account creation / deletion**: the app has no accounts.

Why "No" holds, for the record (Play counts data as collected when it leaves the device, also
to someone else's server, unless it is the user's own, expected action):

- The developer runs no server that receives anything from the app.
- Error reports stay on the device (crash.txt). The app shows the text and offers Copy and
  "Open a GitHub issue", which opens an empty issue page in the browser; the user pastes and
  sends it, or not. Nothing is put in the address.
- The internet requests carry no personal data: the AutoEQ headphone catalog (a plain download
  from bitspring.app when the equalizer's headphone list is opened, at most once a month);
  lyrics from LRCLIB (off by default; when turned on, the playing track's title, artist, album
  and length); the rip check (asked for each time; the lengths of a disc's tracks go to the
  CUETools Database). The privacy page names all three.
- Settings, library, playlists, favourites and ratings stay on the device.

## Notes for review (App access → instructions, if asked)

```
BitSpring is a music player for the user's own audio files. It has no account and no in-app purchases. It goes online only for what the user asks: the equalizer's headphone profiles (a public catalog of AutoEQ corrections from the BitSpring website, fetched at most once a month when the user opens them), song lyrics from LRCLIB if the user turns that on in Settings (off by default), and the rip check, which asks first and sends only the lengths of a CD's tracks to the CUETools Database.

To try it: copy any FLAC, WAV, MP3 or AAC file into the phone's Music folder, or add a folder in the library (tap the album count, then Add folder). A USB DAC is not required: the app plays through the speaker or headphones too, and the indicator then shows the path the signal takes.
```

## Building

```
npm run build:play
```

= `BITSPRING_PLAY=1 tauri android build --aab --target aarch64 -- --no-default-features`
(BITSPRING_PLAY switches build.gradle.kts to the Play package and the overlay manifest in
gen/android/app/src/play/; `--no-default-features` leaves out streaming and the updater, as in
the App Store builds). Android env as for the website APK: ANDROID_HOME, NDK_HOME, JAVA_HOME
on JDK 21. The bundle is
`src-tauri/gen/android/app/build/outputs/bundle/universalRelease/app-universal-release.aab`,
unsigned. Sign it with the upload key (the same keystore as the website APK, used here only as
Play's upload key; Play App Signing holds the key that signs what users install):

```
export PW="$(security find-generic-password -s hifi-android-release-keystore -a hifi-android -w)"
cp src-tauri/gen/android/app/build/outputs/bundle/universalRelease/app-universal-release.aab store/play/BitSpring-0.23.0-play.aab
jarsigner -sigalg SHA256withRSA -digestalg SHA-256 -keystore ~/.android/hifi-release.jks -storepass:env PW store/play/BitSpring-0.23.0-play.aab hifi
jarsigner -verify store/play/BitSpring-0.23.0-play.aab
```

Upload key certificate SHA-256 (Play Console shows it under Setup → App signing):
`37:71:2B:37:0D:AD:CF:A0:BF:C2:6B:3A:48:11:37:4C:6F:A4:D9:AF:D8:D2:71:16:63:0E:D4:C6:AE:A4:6A:C5`.

versionCode comes from the version in tauri.conf.json (0.23.0 → 23000); each upload needs a
higher one, so every Play upload is a new version. `store/play/*.aab` is kept out of git.

To try the Play build on a phone or the emulator, build an APK with the same switch
(`BITSPRING_PLAY=1 npx tauri android build --apk --target aarch64 -- --no-default-features`),
then zipalign `-P 16 4` and sign it with apksigner (the debug keystore will do for a test).
It installs next to the website APK.

---

## Русская версия страницы (ru-RU)

| Поле | Значение |
|---|---|
| Название | BitSpring: Bit-Perfect Player |

Краткое описание:

```
Bit-perfect плеер hi-res: FLAC, DSD, USB-ЦАП и эквалайзер для наушников.
```

Полное описание:

```
BitSpring играет музыку ровно такой, какой она записана. Каждый бит файла доходит до USB-ЦАП без изменений: без пересэмплирования, без подкрутки громкости, без сюрпризов.

Он бесплатный и с открытым кодом. Без аккаунта, подписки, рекламы и встроенных покупок, и о вас ничего не собирается.

BIT-PERFECT
• На Android 14 и новее USB-ЦАП получает каждый трек на его собственной частоте, бит в бит, в обход микшера Android.
• Индикатор честно показывает, идёт ли сигнал bit-perfect или обрабатывается, и почему.
• Воспроизведение без пауз для концертных альбомов, опер и диджей-миксов.

ВСЕ ВАШИ ФОРМАТЫ
• FLAC, ALAC, WAV, AIFF, CAF, MKA, APE, WavPack, TTA, MP3, AAC, Ogg Vorbis и Opus.
• DSD (DSF, DFF и образы SACD): нативно через DoP на ЦАП, которые это умеют, иначе с переводом в PCM.
• Альбомы одним файлом с CUE-листом играют по трекам.
• Обложки, теги и тексты песен: из файлов .lrc и тегов, или с LRCLIB, если вы это включите.

ЗНАЙТЕ, ЧТО У ВАС ЕСТЬ
• Проверка качества: находит «hi-res» файлы, которые на деле апсемплированный CD или сделаны из MP3.
• Динамический диапазон каждого трека и альбома: DR и диапазон громкости EBU R128.
• Проверка рипа: сравнивает рип CD с рипами того же диска других людей в базе CUETools.
• Слепой тест (ABX): проверьте на слух, меняет ли что-то эквалайзер или кроссфид.

УЛУЧШАЙЗЕРЫ — ТОЛЬКО КОГДА ХОТИТЕ
• Параметрический эквалайзер с готовыми коррекциями для тысяч наушников и IEM из AutoEQ: выберите свою модель по названию.
• Кроссфид для более естественной стереопанорамы в наушниках.
• Апсемплинг с выбором фильтра: стандартный, длинный линейно-фазовый, длинный минимально-фазовый и аподизирующий.
• Вывод в DSD: любая музыка превращается в DSD64 и идёт на ЦАП через DoP.
• Выравнивание громкости по трекам или альбомам (теги ReplayGain или замер по EBU R128) и автоматический запас против клиппинга.
Выключите их все — и сигнал снова bit-perfect.

ВАША МЕДИАТЕКА
• Папка Music и любые папки, которые вы добавите: в памяти телефона, на карте памяти или на USB-накопителе. Ненужные папки можно пропустить.
• Избранное, оценки звёздами, умные плейлисты, перемешивание и повтор.
• Плейлисты с сохранением в M3U, PLS, XSPF или CSV.
• Играет с выключенным экраном, с управлением из уведомления и с экрана блокировки.

На 24 языках.
```
