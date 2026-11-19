<script lang="ts">
  import { i18n, LOCALES, t, tn } from "./i18n.svelte";
  import { checkUpdates, updates } from "./updates.svelte";
  import { errorText, inTauri } from "./api";
  import { hasRemote, loadRemote, newAddress, remote, setRemote } from "./remote.svelte";
  import { look, LOOKS } from "./look.svelte";
  import { lyricsPrefs } from "./lyricsPrefs.svelte";
  import { cancelSignIn, connectToken, disconnect, loadScrobble, scrobble, signIn } from "./scrobble.svelte";
  import { liquidPill } from "./liquid";
  import { openUrl } from "@tauri-apps/plugin-opener";

  let {
    fades,
    onfades,
    onclose,
  }: {
    /** Pause, seek and stop fade over a few milliseconds. */
    fades: boolean;
    onfades: (on: boolean) => void;
    /** Back to the screen the settings were opened from. */
    onclose: () => void;
  } = $props();

  const SITE = "https://bitspring.app";
  // In the browser, not in the app's own webview. Outside Tauri (the mock in a browser) the
  // plain link does the job.
  function openSite(e: MouseEvent) {
    if (!inTauri) return;
    e.preventDefault();
    openUrl((e.currentTarget as HTMLAnchorElement).href).catch(() => {});
  }

  // The phone remote is switched on here. Right then its code is shown large, to scan at
  // once; after that it lives under the phone icon in the header.
  $effect(() => {
    loadRemote();
    return () => {
      remote.fresh = false;
    };
  });

  // Scrobbling accounts: kept fresh while the settings are open, so a sign-in approved in the
  // browser, or listens that went out, show without a click.
  const LISTENBRAINZ_TOKEN = "https://listenbrainz.org/settings/";
  let token = $state("");
  $effect(() => {
    loadScrobble();
    const timer = setInterval(loadScrobble, 2000);
    return () => clearInterval(timer);
  });
  function openTokenPage(e: MouseEvent) {
    if (!inTauri) return;
    e.preventDefault();
    openUrl(LISTENBRAINZ_TOKEN).catch(() => {});
  }
  async function submitToken(e: SubmitEvent) {
    e.preventDefault();
    if (!token.trim()) return;
    await connectToken("listenbrainz", token);
    if (!scrobble.error.listenbrainz) token = "";
  }

  function onKey(e: KeyboardEvent) {
    if (e.key !== "Escape" || e.defaultPrevented) return;
    // A select closes its own list on Escape; an open popover in the header (enhancers, the
    // output, the phone remote) takes this press for itself.
    if ((e.target as HTMLElement).tagName === "SELECT" || document.querySelector(".app > header [role='dialog']")) return;
    onclose();
  }
</script>

<svelte:window onkeydown={onKey} />

<!-- One sheet, read top to bottom: what a group is about on the left, its settings on the
     right — the way a device's panel names a row of controls. -->
<div class="settings">
  <div class="sheet">
    <h1>{t("settings.label")}</h1>

    <section aria-labelledby="set-playback">
      <h2 id="set-playback">{t("settings.playback")}</h2>
      <div class="rows">
        <div class="row">
          <div class="what">
            <label for="set-fades">{t("settings.fades")}</label>
            <p class="help">{t("settings.fadesHelp")}</p>
            <!-- The brass lamp is the app's mark for an untouched signal: here it says what
                 this switch leaves alone. -->
            <p class="signal"><i aria-hidden="true"></i>{t("settings.fadesSignal")}</p>
          </div>
          <span class="switch">
            <input id="set-fades" type="checkbox" role="switch" checked={fades} onchange={(e) => onfades(e.currentTarget.checked)} />
            <span class="knob" aria-hidden="true"></span>
          </span>
        </div>
      </div>
    </section>

    <section aria-labelledby="set-appearance">
      <h2 id="set-appearance">{t("settings.appearance")}</h2>
      <div class="rows">
        <div class="row">
          <div class="what"><label for="set-language">{t("settings.language")}</label></div>
          <select id="set-language" value={i18n.pref} onchange={(e) => (i18n.pref = e.currentTarget.value)}>
            <option value="auto">{t("settings.system", { name: i18n.system.name })}</option>
            {#each LOCALES as l (l.code)}
              <option value={l.code} lang={l.tag} dir={l.dir}>{l.name}</option>
            {/each}
          </select>
        </div>
        <div class="row">
          <div class="what"><span id="set-look">{t("settings.look")}</span></div>
          <div class="seg" role="radiogroup" aria-labelledby="set-look" use:liquidPill={look.value}>
            {#each LOOKS as l (l)}
              <button role="radio" aria-checked={look.value === l} class:sel={look.value === l} onclick={() => (look.value = l)}>
                {t(l === "glass" ? "settings.lookGlass" : "settings.lookClassic")}
              </button>
            {/each}
          </div>
        </div>
      </div>
    </section>

    <section aria-labelledby="set-network">
      <h2 id="set-network">{t("settings.network")}</h2>
      <div class="rows">
        <div class="row">
          <div class="what">
            <label for="set-lyrics">{t("lyrics.setting")}</label>
            <p class="help">{t("lyrics.settingHelp")}</p>
          </div>
          <span class="switch">
            <input id="set-lyrics" type="checkbox" role="switch" checked={lyricsPrefs.online} onchange={(e) => (lyricsPrefs.online = e.currentTarget.checked)} />
            <span class="knob" aria-hidden="true"></span>
          </span>
        </div>
        {#if scrobble.services.some((s) => s.available)}
          <div class="row">
            <div class="what">
              <span>{t("scrobble.setting")}</span>
              <p class="help">{t("scrobble.help")}</p>
            </div>
          </div>
          <!-- One row per service, each saying where it stands: nothing connected, waiting
               for the browser, or whose profile the listens go to. -->
          <ul class="accounts">
            {#each scrobble.services.filter((s) => s.available) as s (s.service)}
              {@const busy = scrobble.busy === s.service}
              <li class="row">
                <div class="what">
                  <span>{s.name}</span>
                  {#if s.user}
                    <p class="help">{t("scrobble.connectedAs", { user: s.user })}</p>
                    {#if s.rejected}
                      <p class="help err">{t("scrobble.rejected", { service: s.name })}</p>
                    {:else}
                      {#if s.waiting}<p class="help">{tn("scrobble.waiting", s.waiting)}</p>{/if}
                      {#if s.offline}<p class="help">{t("scrobble.offline", { service: s.name })}</p>{/if}
                    {/if}
                  {:else if s.connecting}
                    <p class="help where">{t("scrobble.approve", { service: s.name })}</p>
                  {:else if s.service === "listenbrainz"}
                    <p class="help">
                      {t("scrobble.tokenHelp")}
                      <a href={LISTENBRAINZ_TOKEN} onclick={openTokenPage}>listenbrainz.org/settings</a>
                    </p>
                    <form class="token" onsubmit={submitToken}>
                      <input
                        type="password"
                        bind:value={token}
                        placeholder={t("scrobble.token")}
                        aria-label={t("scrobble.token")}
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck="false"
                      />
                      <button class="quiet" type="submit" disabled={busy || !token.trim()}>{t("scrobble.connect")}</button>
                    </form>
                  {:else}
                    <p class="help">{t("scrobble.browserHelp", { service: s.name })}</p>
                  {/if}
                  {#if scrobble.error[s.service]}
                    <p class="help err">{scrobble.error[s.service]}</p>
                  {:else if s.failed && !s.user && !s.connecting}
                    <p class="help err">{errorText(s.failed)}</p>
                  {/if}
                </div>
                {#if s.user}
                  {#if s.rejected && s.service !== "listenbrainz"}
                    <button class="quiet" onclick={() => signIn(s.service)} disabled={busy}>{t("scrobble.connect")}</button>
                  {/if}
                  <button class="quiet" onclick={() => disconnect(s.service)} disabled={busy}>{t("scrobble.disconnect")}</button>
                {:else if s.connecting}
                  <button class="quiet" onclick={() => cancelSignIn(s.service)}>{t("pl.cancel")}</button>
                {:else if s.service !== "listenbrainz"}
                  <button class="quiet" onclick={() => signIn(s.service)} disabled={busy}>{t("scrobble.connect")}</button>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
        {#if hasRemote && remote.status}
          {@const r = remote.status}
          <div class="row">
            <div class="what">
              <label for="set-remote">{t("remote.setting")}</label>
              <p class="help">{t("remote.help")}</p>
              {#if r.error}<p class="help err">{t("remote.failed", { error: r.error })}</p>{/if}
              {#if r.enabled && r.url && !remote.fresh}<p class="help where">{t("remote.where")}</p>{/if}
              {#if r.enabled && r.url}
                <p class="help">{t("remote.api")} <a href={`${SITE}/api/`} onclick={openSite}>{t("remote.apiLink")}</a></p>
              {/if}
            </div>
            <span class="switch">
              <input id="set-remote" type="checkbox" role="switch" checked={r.enabled} onchange={(e) => setRemote(e.currentTarget.checked)} />
              <span class="knob" aria-hidden="true"></span>
            </span>
          </div>
          {#if r.enabled && r.url && remote.fresh}
            <div class="pairing">
              <!-- Our own SVG, made by the app from the address. -->
              <div class="qr" aria-hidden="true">{@html r.qr}</div>
              <div class="address">
                <p class="scan">{t("remote.scan")}</p>
                <p class="url">{r.url}</p>
                <button class="quiet" onclick={newAddress}>{t("remote.newAddress")}</button>
                <p class="help">{t("remote.newHelp")}</p>
                <p class="help">{t("remote.where")}</p>
              </div>
            </div>
          {/if}
        {/if}
      </div>
    </section>

    <section aria-labelledby="set-about">
      <h2 id="set-about">{t("settings.about")}</h2>
      <div class="rows">
        <div class="row">
          <div class="what">
            <span>BitSpring{#if updates.current}&nbsp;{updates.current}{/if}</span>
            {#if updates.enabled}
              {#if updates.update}
                <p class="help new">{t("upd.available", { version: updates.update.version })}</p>
              {:else if updates.upToDate}
                <p class="help">{t("upd.upToDate")}</p>
              {/if}
              {#if updates.error && !updates.update}<p class="help err">{updates.error}</p>{/if}
            {/if}
            <p class="help"><a href={SITE} onclick={openSite}>bitspring.app</a></p>
          </div>
          {#if updates.enabled}
            {#if updates.update}
              {#if updates.dismissed === updates.update.version}
                <button class="quiet" onclick={() => (updates.dismissed = null)}>{t("upd.show")}</button>
              {/if}
            {:else}
              <button class="quiet" onclick={() => checkUpdates(true)} disabled={updates.checking}>
                {t(updates.checking ? "upd.checking" : "upd.check")}
              </button>
            {/if}
          {/if}
        </div>
      </div>
    </section>
  </div>
</div>

<style>
  .settings {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
  }
  .sheet {
    /* The settings' own measure: a legend column, a reading column, and one width for the
       wide controls at its end. */
    --legend: 168px;
    --control: 208px;
    max-width: 780px;
    padding: 26px var(--gutter) 48px;
  }
  h1 {
    margin: 0 0 6px;
    font-family: var(--font-display);
    font-size: var(--t-xl);
    font-weight: 600;
    letter-spacing: 0.01em;
  }

  /* A group: its name in the legend column, its settings beside it, a rule above. */
  section {
    display: grid;
    grid-template-columns: var(--legend) minmax(0, 1fr);
    column-gap: 20px;
    padding: 20px 0 22px;
    border-top: 1px solid var(--slate-600);
  }
  h1 + section {
    margin-top: 16px;
  }
  h2 {
    margin: 0;
    font-family: var(--font-display);
    font-size: var(--t-md);
    font-weight: 600;
    letter-spacing: 0.02em;
    color: var(--mist);
    /* on the first row's baseline */
    line-height: 1.45;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 18px;
    min-width: 0;
  }
  .row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px 24px;
  }
  .what {
    flex: 1;
    min-width: 0;
    max-width: 46ch;
  }
  .what > label,
  .what > span {
    font-weight: 600;
  }
  .what > label {
    cursor: pointer;
  }
  .help {
    margin: 4px 0 0;
    font-size: var(--t-sm);
    line-height: 1.5;
    color: var(--mist);
  }
  .help.new {
    color: var(--porcelain);
  }
  .help.err {
    color: var(--alarm);
  }
  .help a {
    color: var(--porcelain);
    text-decoration: underline;
    text-decoration-color: var(--slate-500);
    text-underline-offset: 3px;
  }
  .help a:hover {
    text-decoration-color: var(--porcelain);
  }

  /* What a switch leaves alone, under the app's lamp for an untouched signal. */
  .signal {
    display: flex;
    gap: 9px;
    margin: 8px 0 0;
    font-size: var(--t-sm);
    line-height: 1.5;
    color: var(--porcelain);
  }
  .signal i {
    flex: none;
    width: 7px;
    height: 7px;
    margin-top: 0.5em;
    border-radius: 50%;
    background: var(--brass);
    box-shadow: 0 0 8px var(--brass-glow);
  }

  /* On/off. Porcelain when on: brass and violet are kept for the signal. */
  .switch {
    position: relative;
    flex: none;
    display: block;
    width: 38px;
    height: 22px;
    margin-top: 1px;
  }
  .switch input {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    margin: 0;
    opacity: 0;
    cursor: pointer;
  }
  .knob {
    position: absolute;
    inset: 0;
    border-radius: 999px;
    background: var(--slate-800);
    border: 1px solid var(--slate-500);
    pointer-events: none;
    transition:
      background 120ms ease,
      border-color 120ms ease;
  }
  .knob::after {
    content: "";
    position: absolute;
    top: 3px;
    inset-inline-start: 3px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--mist);
    transition:
      transform 140ms ease,
      background 120ms ease;
  }
  .switch input:checked + .knob {
    background: rgba(232, 237, 234, 0.16);
    border-color: var(--porcelain);
  }
  .switch input:checked + .knob::after {
    transform: translateX(16px);
    background: var(--porcelain);
  }
  :global([dir="rtl"]) .switch input:checked + .knob::after {
    transform: translateX(-16px);
  }
  .switch input:focus-visible + .knob {
    outline: 2px solid var(--porcelain);
    outline-offset: 2px;
  }

  /* The wide controls — a list to pick from, a row of choices — are one size and one shape,
     so a column of them lines up on both edges. */
  select,
  .seg {
    flex: none;
    box-sizing: border-box;
    width: var(--control);
    height: 38px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    font-size: var(--t-sm);
  }
  select {
    --select-chevron: 34px;
    padding: 0 14px;
    background-color: var(--slate-800);
    background-position: right 13px center;
  }
  :global([dir="rtl"]) select {
    background-position: left 13px center;
  }
  :global(html[data-look="glass"]) select {
    border-radius: 999px;
    padding: 0 16px;
  }
  .seg {
    display: flex;
    padding: 3px;
    gap: 2px;
  }
  .seg button {
    flex: 1;
    display: grid;
    place-items: center;
    padding: 0 10px;
    border-radius: var(--r-md);
    color: var(--mist);
    white-space: nowrap;
  }
  .seg button:hover {
    color: var(--porcelain);
  }
  .seg button.sel {
    color: var(--porcelain);
    background: var(--slate-600);
  }
  :global(html[data-look="glass"]) .seg,
  :global(html[data-look="glass"]) .seg button {
    border-radius: 999px;
  }
  .quiet {
    flex: none;
    padding: 6px 12px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-size: var(--t-sm);
    white-space: nowrap;
  }
  .quiet:hover:not(:disabled) {
    border-color: var(--mist);
  }
  .quiet:disabled {
    color: var(--mist);
  }

  /* The scrobbling accounts belong to the row above them: closer to it, and to each other,
     than rows of their own would be. */
  .accounts {
    list-style: none;
    margin: -6px 0 0;
    /* a rule down their side: these are the services of the setting above */
    padding: 2px 0 2px 14px;
    padding-inline: 14px 0;
    border-inline-start: 1px solid var(--slate-600);
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .accounts .row {
    align-items: center;
    flex-wrap: wrap;
  }
  .token {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .token input {
    flex: 1;
    min-width: 0;
    max-width: var(--control);
    height: 32px;
    padding: 0 10px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    background: var(--slate-800);
    color: var(--porcelain);
    font: inherit;
    font-size: var(--t-sm);
  }
  .token input:focus {
    border-color: var(--mist);
    outline: none;
  }
  :global(html[data-look="glass"]) .token input {
    border-radius: 999px;
    padding: 0 14px;
  }

  /* The phone remote's way in: the code to scan, and the same address in words. */
  .pairing {
    display: flex;
    align-items: flex-start;
    gap: 18px;
  }
  .qr {
    flex: none;
    /* large: this is the moment to point a phone at it */
    width: 224px;
    padding: 12px;
    border-radius: var(--r-md);
    background: #fff;
    line-height: 0;
  }
  .qr :global(svg) {
    width: 100%;
    height: auto;
  }
  .address {
    min-width: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
  }
  .address .quiet {
    margin-top: 10px;
  }
  .scan {
    margin: 0 0 10px;
    font-weight: 600;
  }
  .help.where {
    color: var(--porcelain);
  }
  .url {
    margin: 0;
    font-size: var(--t-sm);
    color: var(--porcelain);
    user-select: all;
    -webkit-user-select: all;
    word-break: break-all;
  }

  @media (prefers-reduced-motion: reduce) {
    .knob,
    .knob::after {
      transition: none;
    }
  }

  /* A narrow window, a phone: the group's name above its settings. */
  @media (max-width: 1000px) {
    .sheet {
      --legend: 132px;
    }
  }
  @media (max-width: 700px), (max-height: 500px) {
    .sheet {
      padding: 18px 16px 32px;
    }
    section {
      grid-template-columns: 1fr;
      row-gap: 12px;
      padding: 16px 0 18px;
    }
    .pairing {
      flex-direction: column;
    }
  }
</style>
