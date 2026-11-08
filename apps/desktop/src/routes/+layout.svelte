<script lang="ts">
  import "@fontsource/barlow/400.css";
  import "@fontsource/barlow/500.css";
  import "@fontsource/barlow/600.css";
  import "@fontsource/barlow-semi-condensed/500.css";
  import "@fontsource/barlow-semi-condensed/600.css";
  import "$lib/theme.css";

  let { children } = $props();
</script>

{@render children()}
