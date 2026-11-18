# BitSpring in the Microsoft Store

The Store build is an MSIX package (`npm run build:msstore`, Windows only; the
`Microsoft Store package` GitHub workflow builds it on windows-latest). Like the App Store
builds it leaves out Qobuz/TIDAL and self-updating: the Store delivers updates. Don't mention
streaming in the listing.

The Store re-signs MSIX packages, so no code-signing certificate is needed. The package is a
full-trust desktop app (`runFullTrust`): WASAPI exclusive mode, any music folder and the media
keys work as in the website build. Its settings and library live apart from the website
build's (identifier `app.bitspring.player`, and MSIX keeps each package's AppData separately).

## Steps

1. **Account.** Sign up at https://storedeveloper.microsoft.com with a personal Microsoft
   account (individual developers: free; identity check with an ID document).
2. **Reserve the name.** Partner Center → Apps and games → New product → **MSIX or PWA app** →
   name `BitSpring` (the listing's longer title is set later, per language).
3. **Identity.** Product management → Product identity: copy `Package/Identity/Name`,
   `Package/Identity/Publisher` and `Package/Properties/PublisherDisplayName` into
   `apps/desktop/src-tauri/msstore/identity.json`, commit, and the workflow builds a package
   with them.
4. **Package.** Download the `BitSpring-msix-x64` artifact from the workflow run and upload the
   `.msix` under Packages. Each new submission needs a higher version: the workflow takes it
   from tauri.conf.json (0.18.0 → 0.18.0.0).
5. **Listing, age rating, pricing** as below. Submission options asks why the app needs
   runFullTrust (500 characters at most): it runs as a normal desktop process for WASAPI exclusive
   mode, any music folder, media keys and network players. Then submit. Certification takes up to three
   business days.

Before submitting, check the workflow's **App Certification Kit** step and `wack-report`
artifact: these are the Store's own technical tests. The `launch-screenshot` artifact shows
the app after installing the package on a clean Windows.

## Properties

| Field | Value |
|---|---|
| Category | Music |
| Subcategory | — |
| Privacy policy URL | https://bitspring.app/privacy/ |
| Website | https://bitspring.app/ |
| Support contact | https://github.com/crash2bash/bitspring/issues |
| Product declarations | Leave the defaults (no accessibility claim, installs on up to 10 devices) |
| System requirements | Minimum: Windows 10 version 1809, x64. Recommended: a USB DAC |
| Pricing | Free, all markets |
| Age rating (IARC questionnaire) | Category "Utility/Productivity"; answer No to everything → 3+ / Everyone |

## Listing (English)

| Field | Value |
|---|---|
| Product name | BitSpring (must be the reserved name exactly) |
| Short description | Bit-perfect hi-res music player for your own FLAC, ALAC and DSD files, with EQ for your headphones. |
| Search terms (7) | hifi, audiophile, flac, dac, lossless, dsd, wasapi exclusive |
| Copyright | 2026 Oleksandr Hurov |

### Description

```
BitSpring plays your music exactly as it was recorded. Every bit of the file reaches your DAC unchanged: no resampling, no Windows mixer, no surprises.

It's free and open source. No account, no subscription, no ads, no in-app purchases, and nothing about you is collected.

BIT-PERFECT
• WASAPI exclusive mode: no other app or system sound gets mixed in, and your DAC switches to each track's sample rate, up to 768 kHz.
• A clear indicator shows whether the signal is bit-perfect or being processed, and why.
• Gapless playback for live albums, operas and DJ mixes.
• Network players: Sonos, WiiM, Bluesound and other UPnP/DLNA streamers, and Chromecast and Google Cast devices, get your music as FLAC, sample for sample and without gaps.
• Phone remote: browse the library and control playback from any phone on your network, no app to install.

EVERY FORMAT YOU OWN
• FLAC, ALAC, WAV, AIFF, CAF, MKA, APE, WavPack, TTA, MP3, AAC and Ogg Vorbis.
• DSD (DSF, DFF and SACD disc images) up to DSD1024: native over DoP on DACs that support it, or converted to 176.4 kHz PCM.
• Album art, tags, and a library that watches your music folders.
• Albums ripped to one file with a CUE sheet play track by track.
• Quality check: finds "hi-res" files that are upsampled CDs or were made from MP3, right on your PC.

ENHANCERS, ONLY WHEN YOU WANT THEM
• Parametric EQ in linear or minimum phase, with ready-made corrections for thousands of headphones and IEMs from AutoEQ: pick your model by name. It also imports Equalizer APO, EasyEffects and AutoEQ files.
• Crossfeed for a more natural stereo image on headphones.
• High-quality upsampling.
• For lossy files: automatic headroom against clipping.
Turn them all off and the signal is bit-perfect again.

YOUR LIBRARY
• Add any music folders, on internal or external drives.
• Playlists: import M3U, PLS, XSPF and WPL, export M3U, PLS, XSPF and CSV.
• Media keys and the Windows volume flyout control playback.

In 24 languages.
```

### Product features (up to 20, 200 characters each)

```
Bit-perfect playback with WASAPI exclusive mode
Switches your DAC to each track's sample rate, up to 768 kHz
Native DSD over DoP, up to DSD1024
FLAC, ALAC, WAV, AIFF, APE, WavPack, MP3, AAC and more
Gapless playback and CUE sheets
Sonos, WiiM, UPnP/DLNA and Chromecast network players
Parametric EQ with AutoEQ corrections for thousands of headphones
Quality check for fake hi-res files
Free and open source: no account, no ads, no tracking
```

### Notes for certification

Goes in Additional testing info (left menu), not in the listing's license terms, which customers see.

```
BitSpring is a music player for the user's own audio files. It has no account and no in-app purchases. It uses the internet in two places only: the equalizer's headphone profiles (a public catalog of AutoEQ corrections from the BitSpring website, fetched at most once a month when the user opens them) and, if the user turns it on in Settings (off by default), song lyrics from LRCLIB for the playing track. On the local network it finds UPnP/DLNA and Google Cast players and streams the user's music to them; the phone remote (off by default) serves a small control page to phones on the same network.

To try it: press "Add files" and pick some music files (FLAC, WAV, MP3 or AAC), or drop an album folder onto the window. A USB DAC is not required: it plays through any output device, and the indicator shows the path the signal takes.
```

### Images

- Screenshots: `screenshots/windows`, four 2880 × 1620 PNGs (the Store takes 1366 × 768 and up,
  1 to 10), in the glass look. The listing shows them as Bit-perfect playing, EQ, Processing,
  Library. Retake after UI changes: `npm run dev`, then `node scripts/store-screenshots.mjs windows`.
  To swap them in a later submission, each screenshot tile has its own file input: replacing in
  place keeps the caption. They come from the mock backend dressed as the
  Store build (no streaming tab), and the album covers are our own abstract art
  (src/lib/mockCovers.ts): never use real album covers, they are other people's artwork.
- Store logos (optional, for Windows 11 Store pages): 1:1 box art 1080 × 1080 and 2:3 poster
  720 × 1080, both from the icon masters (`src-tauri/icons/source`).

Other languages: Partner Center adds a listing for each of the 24 languages the package declares
and wants every one filled. The texts for all of them are in `msstore-listing/<language>.json`
(English is the source); they go in through Store listings → Export listings / Import listings.
