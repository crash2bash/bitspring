# BitSpring in the App Store

What goes into App Store Connect for the iPhone app and the Mac App Store app (one record,
two platforms). English is the primary language; the Russian listing follows.

Both App Store builds leave out streaming (Qobuz, TIDAL) and self-updating: they play the
user's own files only. Don't mention streaming anywhere in the listing.

## App information

| Field | Value |
|---|---|
| Name | BitSpring: Bit-Perfect Player |
| Subtitle | Hi-Res Music Player, DSD & EQ |
| Bundle ID | app.bitspring.player (iPhone and Mac) |
| SKU | bitspring |
| Primary category | Music |
| Secondary category | Utilities |
| Price | Free, no in-app purchases |
| Age rating | 4+ (answer "None" to every question) |
| Privacy policy URL | https://hifi-player-nu.vercel.app/privacy/ |
| Support URL | https://github.com/crash2bash/bitspring/issues |
| Marketing URL | https://hifi-player-nu.vercel.app/ |
| Copyright | 2026 Oleksandr Hurov |
| Content rights | Contains no third-party content |
| Export compliance | Uses only exempt encryption (ITSAppUsesNonExemptEncryption = NO in the builds) |

App Privacy: **Data Not Collected**.

Keywords (93 of 100 characters; words already in the name and subtitle are indexed anyway):

```
hifi,audiophile,flac,dac,lossless,alac,usb,exclusive,crossfeed,upsampling,cue,wav,dop,offline
```

## Promotional text (170 max, can change without review)

```
Free, with no subscriptions, no ads and no in-app purchases. Play your own FLAC, ALAC and DSD files bit for bit, and add EQ for your headphones only when you want it.
```

## Description (iPhone)

```
BitSpring plays your music exactly as it was recorded. Every bit of the file reaches your DAC unchanged: no resampling, no volume tricks, no surprises.

It's free and open source. No account, no subscription, no ads, no in-app purchases, and nothing about you is collected.

BIT-PERFECT
• On a USB DAC each track plays at its own sample rate, as far as the DAC and iOS allow.
• A clear indicator shows whether the signal is bit-perfect or being processed, and why.
• Gapless playback for live albums, operas and DJ mixes.

EVERY FORMAT YOU OWN
• FLAC, ALAC, WAV, AIFF, CAF, MKA, APE, WavPack, TTA, MP3, AAC and Ogg Vorbis.
• DSD (DSF, DFF and SACD disc images): native over DoP on DACs that support it, or converted to 176.4 kHz PCM.
• Album art, tags and a library that finds new files by itself.
• Albums ripped to one file with a CUE sheet play track by track.
• Multichannel files (5.1, 7.1) play channel for channel on multichannel DACs and receivers, or are mixed down to stereo without clipping.
• Quality check: finds "hi-res" files that are upsampled CDs or were made from MP3, right on your device.

ENHANCERS, ONLY WHEN YOU WANT THEM
• Parametric EQ with ready-made corrections for thousands of headphones (AutoEQ); imports Equalizer APO and EasyEffects presets.
• Crossfeed for a more natural stereo image on headphones.
• High-quality upsampling.
• For lossy files: automatic headroom against clipping.
Turn them all off and the signal is bit-perfect again.

YOUR LIBRARY
• Copy albums into Files → On My iPhone → BitSpring, or from a Mac with Finder.
• Playlists: import M3U, PLS, XSPF and WPL, export M3U, PLS, XSPF and CSV.
• Keeps playing with the screen locked, with lock-screen controls.

In 24 languages.
```

## Description (Mac)

```
BitSpring plays your music exactly as it was recorded. Every bit of the file reaches your DAC unchanged: no resampling, no system mixer, no surprises.

It's free and open source. No account, no subscription, no ads, no in-app purchases, and nothing about you is collected.

BIT-PERFECT
• Switches your DAC to each track's sample rate, up to 768 kHz.
• Exclusive (hog) mode: no other app or system sound gets mixed in.
• A clear indicator shows whether the signal is bit-perfect or being processed, and why.
• Gapless playback for live albums, operas and DJ mixes.
• Network players: Sonos, WiiM, Bluesound and other UPnP/DLNA streamers, and Chromecast and Google Cast devices, get your music as FLAC, sample for sample and without gaps.
• Phone remote: browse the library and control playback from any phone on your network, no app to install.

EVERY FORMAT YOU OWN
• FLAC, ALAC, WAV, AIFF, CAF, MKA, APE, WavPack, TTA, MP3, AAC and Ogg Vorbis.
• DSD (DSF, DFF and SACD disc images) up to DSD1024: native over DoP on DACs that support it, or converted to 176.4 kHz PCM.
• Album art, tags, and a library that watches your music folders.
• Albums ripped to one file with a CUE sheet play track by track.
• Quality check: finds "hi-res" files that are upsampled CDs or were made from MP3, right on your device.

ENHANCERS, ONLY WHEN YOU WANT THEM
• Parametric EQ in linear or minimum phase, with ready-made corrections for thousands of headphones and IEMs from AutoEQ: pick your model by name. It also imports Equalizer APO, EasyEffects and AutoEQ files.
• Crossfeed for a more natural stereo image on headphones.
• High-quality upsampling.
• For lossy files: automatic headroom against clipping.
Turn them all off and the signal is bit-perfect again.

YOUR LIBRARY
• Add any music folders, on internal or external drives. BitSpring remembers them.
• Playlists: import M3U, PLS, XSPF and WPL, export M3U, PLS, XSPF and CSV.

In 24 languages.
```

## What's New (first release)

```
First release in the App Store.
```

## Notes for App Review

```
BitSpring is a music player for the user's own audio files. It has no account and no in-app purchases. Its only internet use is the equalizer's headphone profiles: when the user opens them, it downloads a catalog of AutoEQ headphone corrections (a public JSON file) from the BitSpring website, at most once a month; nothing is sent besides the request itself. Everything else is on the local network: it finds UPnP/DLNA and Google Cast network players (Sonos, WiiM, Chromecast and the like) and streams the user's music to them, which is why it asks for local network access. When the user turns on the phone remote in Settings, it also serves a small control page to phones on the same network (off by default, protected by a random address).

To try it: on iPhone, add audio files with the "Add files" button in the library (any FLAC, WAV, MP3 or AAC file works) or copy them into Files → On My iPhone → BitSpring. On Mac, press "Add folder" and pick any folder with music.

A USB DAC is not required: the app plays through the built-in speaker or headphones too, and the indicator then shows the path the signal takes.
```

## Screenshots

- iPhone: 6.9" display (1320 × 2868), 3 to 10 screenshots. Taken in the simulator on an
  iPhone 17 Pro Max with real-looking albums.
- Mac: 2880 × 1800 (16:10), 1 to 10 screenshots.

---

## Русская версия страницы

| Поле | Значение |
|---|---|
| Название | BitSpring: Bit-Perfect Player |
| Подзаголовок | Hi-Res плеер, DSD и эквалайзер |

Ключевые слова:

```
hifi,аудиофил,flac,цап,lossless,alac,usb,эквалайзер,кроссфид,апсемплинг,wav,dsd,dop,офлайн,плеер
```

Описание и рекламный текст переводятся с английских, когда английская версия утверждена.
