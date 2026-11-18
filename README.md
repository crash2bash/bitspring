<a href="https://bitspring.app">
  <img src=".github/readme/banner.png" alt="BitSpring. Bit for bit: a free, bit-perfect music player for Mac, Windows, Linux and Android" width="100%">
</a>

<p align="center">
  <a href="https://bitspring.app/download/"><b>Download</b></a>
  &nbsp;&nbsp;|&nbsp;&nbsp;
  <a href="https://bitspring.app">Website</a>
  &nbsp;&nbsp;|&nbsp;&nbsp;
  <a href="https://github.com/crash2bash/bitspring/releases">What's new</a>
  &nbsp;&nbsp;|&nbsp;&nbsp;
  <a href="#building">Build from source</a>
</p>

<p align="center">
  <a href="https://github.com/crash2bash/bitspring/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/crash2bash/bitspring?style=flat-square&label=release&labelColor=16232a&color=c9a45c"></a>
  <img alt="macOS, Windows, Linux, Android" src="https://img.shields.io/badge/runs_on-macOS_%7C_Windows_%7C_Linux_%7C_Android-2b414c?style=flat-square&labelColor=16232a">
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/github/license/crash2bash/bitspring?style=flat-square&labelColor=16232a&color=2b414c"></a>
  <img alt="Written in Rust" src="https://img.shields.io/badge/engine-Rust-2b414c?style=flat-square&logo=rust&logoColor=e8edea&labelColor=16232a">
</p>

BitSpring is a free, open-source music player that sends your DAC exactly what is in the file: no
resampling, no system mixer, no surprises. EQ, crossfeed and upsampling are there when you want
them, and off means bit-perfect again.

<img src=".github/readme/hero.png" alt="BitSpring on a Mac and on an Android phone: the queue, the track playing, and the Bit-perfect indicator with the signal path from the file to the headphones" width="100%">

## What it does

- **Bit-perfect by default.** The player takes the DAC for itself: exclusive (hog) mode with
  integer output on the Mac, WASAPI Exclusive on Windows, the bit-perfect USB path on Android 14
  and later, and ALSA straight to the sound card on Linux.
- **Every track at its own sample rate**, from CD to 768 kHz, with gapless playback. If the DAC
  can't do a rate, the player resamples carefully and says so.
- **The formats you have:** FLAC, ALAC, WAV, AIFF, CAF, MKA, APE, WavPack, TTA, MP3, AAC and Ogg
  Vorbis. DSD (DSF, DFF, SACD ISO) plays natively over DoP, or as PCM if the DAC can't take it.
- **Sound shaping when you ask for it:** parametric EQ with AutoEQ headphone profiles and Equalizer
  APO / EasyEffects import, crossfeed, and upsampling to 192, 384 or 768 kHz. Turn any of them on
  and the indicator switches from *Bit-perfect* to *Processing* and lists what is being done.
- **Volume levelling** by track or by album: one fixed gain per track towards −18 LUFS, nothing
  compressed. The player reads ReplayGain tags, measures the files that have none (EBU R128), and
  can write the result into their tags. Off by default.
- **Quality check** looks at a file's spectrum and tells you when "hi-res" is really a CD or an MP3
  inside a bigger file.
- **Nothing to sign up for:** no account, no ads, no in-app purchases, nothing collected.

<table>
  <tr>
    <td width="50%"><img src=".github/readme/screens/library-glass.webp" alt="Library: albums with their bit depth and sample rate"></td>
    <td width="50%"><img src=".github/readme/screens/eq-glass.webp" alt="Equalizer: a six-band parametric EQ curve for planar headphones"></td>
  </tr>
  <tr>
    <td align="center">Library, with each album's bit depth and sample rate</td>
    <td align="center">Equalizer on: the indicator turns violet and reads <i>Processing</i></td>
  </tr>
</table>

## Download

| Platform | Get it |
| --- | --- |
| **Mac**, Apple Silicon and Intel | [BitSpring-mac.dmg](https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-mac.dmg) |
| **Windows** 10 and 11 | [Microsoft Store](https://apps.microsoft.com/detail/9NDVJBXLSHVM) (no Qobuz or TIDAL) or [BitSpring-windows-setup.exe](https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-windows-setup.exe) |
| **Linux**, any distribution | [AppImage](https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-linux.AppImage) (updates itself) or [.deb](https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-linux.deb) for Ubuntu, Debian and Mint |
| **Android** 9 and later | [BitSpring-android.apk](https://github.com/crash2bash/bitspring/releases/latest/download/BitSpring-android.apk) |
| **iPhone** and iPad | Coming to the App Store |

Install notes for each platform are at [bitspring.app/download](https://bitspring.app/download/).

## Building

Needs Rust and Node.js. From `apps/desktop`:

```sh
npm ci
npx tauri dev                 # run the desktop app
npx tauri build               # installer for this computer
npm run build:appstore        # Mac App Store build (sandboxed, no streaming or self-update)
```

<details>
<summary><b>What's where</b></summary>

- `crates/engine`: the audio engine (decoding, DSP, CoreAudio / WASAPI / AAudio / ALSA / iOS output)
- `crates/streaming`: Qobuz and TIDAL (website builds only)
- `crates/cli`: a command-line player for testing the engine
- `apps/desktop`: the app (Tauri 2 + Svelte 5) for Mac, Windows, Linux, iPhone and Android
- `apps/website`: the website
- `.github/readme`: the images on this page and the HTML they are rendered from

</details>

## License

[MIT](LICENSE). Third-party libraries keep their own licenses (mostly MIT and Apache-2.0; the
Symphonia decoders are MPL-2.0).
