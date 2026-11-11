# BitSpring

A free, open-source music player that sends your DAC exactly what is in the file: no resampling, no
system mixer, no surprises. EQ, crossfeed and upsampling are there when you want them, and off
means bit-perfect again.

- **Mac** (Apple Silicon and Intel), **Windows** 10/11, **Android** 9+ and **iPhone**
- FLAC, ALAC, WAV, AIFF, CAF, MKA, MP3, AAC, Ogg Vorbis; DSD (DSF, DFF) native over DoP or as PCM
- Each track at its own sample rate, up to 768 kHz; exclusive (hog) mode on the Mac; gapless
- Parametric EQ with AutoEQ, Equalizer APO and EasyEffects import; crossfeed; upsampling
- No account, no ads, no in-app purchases, nothing collected

**Download:** https://hifi-player-nu.vercel.app/download/ or the
[latest release](https://github.com/crash2bash/bitspring/releases/latest).

## Layout

- `crates/engine`: the audio engine (decoding, DSP, CoreAudio / WASAPI / AAudio / iOS output)
- `crates/streaming`: Qobuz and TIDAL (website builds only)
- `crates/cli`: a command-line player for testing the engine
- `apps/desktop`: the app (Tauri 2 + Svelte 5) for Mac, Windows, iPhone and Android
- `apps/website`: the website

## Building

Needs Rust and Node.js. From `apps/desktop`:

```
npm ci
npx tauri dev                 # run the desktop app
npx tauri build               # installer for this computer
npm run build:appstore        # Mac App Store build (sandboxed, no streaming or self-update)
```

## License

[MIT](LICENSE). Third-party libraries keep their own licenses (mostly MIT and Apache-2.0; the
Symphonia decoders are MPL-2.0).
