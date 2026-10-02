// Typed bridge to the Rust commands in src-tauri/src/lib.rs.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface Targets {
  targetLufs: number;
  truePeakDb: number;
}

export interface AudioTrack {
  index: number;
  codec: string;
  sampleRate: number;
  channels: number;
  language: string;
  title: string;
}

export interface FileInfo {
  path: string;
  name: string;
  directory: string;
  size: number;
  audioTracks: AudioTrack[];
}

export interface Measurement {
  integratedLufs: number;
  loudnessRange: number;
  truePeakDb: number;
  samplePeakDb: number;
  maxShortTermLufs?: number;
  maxMomentaryLufs?: number;
}

export interface MediaSummary {
  duration: number;
  codec: string;
  sampleRate: number;
  channels: number;
  hasVideo: boolean;
}

export type Verdict = "none" | "small" | "moderate" | "large";

export interface Assessment {
  targetLufs: number;
  truePeakCeilingDb: number;
  gainDb: number;
  gainCapped: boolean;
  expectedLufs: number;
  peakAfterGainDb: number;
  limiterReductionDb: number;
  deviationLu: number;
  peakOverCeiling: boolean;
  clipping: boolean;
  improvementPercent: number;
  verdict: Verdict;
}

export interface AnalysisReport {
  media: MediaSummary;
  measurement: Measurement;
  assessment: Assessment;
}

export type OutputMode = "replace" | "copy";

export interface NormalizeOptions {
  output: OutputMode;
  track: number;
  allTracks: boolean;
  leveling: boolean;
  lossless: boolean;
}

export interface NormalizeReport {
  path: string;
  outputPath: string;
  replaced: boolean;
  tracksProcessed: number;
  leveled: boolean;
  inputLufs: number;
  inputTruePeakDb: number;
  inputLoudnessRange: number;
  inputMaxShortTermLufs?: number;
  inputMaxMomentaryLufs?: number;
  outputLufs: number;
  outputTruePeakDb: number;
  targetLufs: number;
  truePeakCeilingDb: number;
  gainDb: number;
  limiterMaxReductionDb: number;
  elapsedSeconds: number;
  sizeBefore: number;
  sizeAfter: number;
  inputMedia: MediaSummary;
  outputMedia: MediaSummary;
}

export type Stage = "analyze" | "calibrate" | "normalize" | "retry" | "verify" | "finalize";

export interface ProgressEvent {
  stage: Stage;
  percent: number;
}

type CommandError =
  | { kind: "busy" }
  | { kind: "cancelled" }
  | { kind: "failed"; message: string };

export const api = {
  inspectFile: (path: string) => invoke<FileInfo>("inspect_file", { path }),
  analyze: (path: string, targets: Targets, track: number) =>
    invoke<AnalysisReport>("analyze_file", { path, targets, track }),
  assess: (measurement: Measurement, targets: Targets) =>
    invoke<Assessment>("assess", { measurement, targets }),
  normalize: (path: string, targets: Targets, measured: Measurement | null, options: NormalizeOptions) =>
    invoke<NormalizeReport>("normalize_file", { path, targets, measured, options }),
  cancel: () => invoke<void>("cancel_job"),
  onProgress: (handler: (event: ProgressEvent) => void): Promise<UnlistenFn> =>
    listen<ProgressEvent>("job-progress", (e) => handler(e.payload)),
};

export function isCancelled(err: unknown): boolean {
  return (err as CommandError | undefined)?.kind === "cancelled";
}

export function errorMessage(err: unknown): string {
  const e = err as CommandError | string | undefined;
  if (typeof e === "string") {
    return e;
  }
  switch (e?.kind) {
    case "busy":
      return "Já existe um processamento em andamento.";
    case "cancelled":
      return "Processamento cancelado.";
    case "failed":
      return capitalize(e.message);
    default:
      return "Erro inesperado.";
  }
}

function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}
