// Words for the quality check's results (the check itself runs in the engine: quality.rs).
import type { QualityReport, QualityVerdict } from "./api";
import { num, t } from "./i18n.svelte";

/** Verdicts that say the file is less than it claims. */
export const SUSPECT: QualityVerdict[] = ["upsampled", "fromLossy", "padded"];

export function verdictLabel(kind: QualityVerdict): string {
  return t(`q.v.${kind}`);
}

const khz = (hz: number) => num(hz / 1000, { maximumFractionDigits: 1 });

/** One or two sentences on what the check found, in plain words. */
export function verdictDetail(r: QualityReport): string {
  const cut = r.cutoffHz != null ? khz(r.cutoffHz) : "";
  switch (r.verdict.kind) {
    case "genuine":
      if (r.cutoffHz == null) return t("q.d.genuineFull");
      if (r.cutoffHz > 22_500) return t("q.d.genuineHigh", { khz: cut });
      return t("q.d.genuineFade", { khz: cut });
    case "upsampled":
      // No wall: what gave it away is the music's mirror image above the old file's limit.
      if (!r.wall && r.cutoffHz != null)
        return t("q.d.mirror", {
          khz: cut,
          rate: khz(r.sampleRate),
          from: khz(r.cutoffHz * 2),
        });
      return t("q.d.upsampled", { khz: cut, rate: khz(r.sampleRate) });
    case "fromLossy":
      return t("q.d.fromLossy", { khz: cut });
    case "padded":
      return t("q.d.padded");
    case "lossy":
      return t("q.d.lossy");
    case "dsd":
      return t("q.d.dsd");
    case "inconclusive":
      if (r.analysedSecs < 3) return t("q.d.short");
      if (r.wall) return t("q.d.wallAmbiguous", { khz: cut });
      return t("q.d.gradualLow", { khz: cut });
  }
}
