import { getCurrentWebview } from "@tauri-apps/api/webview";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { check } from "@tauri-apps/plugin-updater";
import { isFinished, overallPercent, summarize, type QueueStatus } from "./batch";
import { applyLanguage, currentLanguage, decimal, t, type Language } from "./i18n";
import {
  api,
  errorMessage,
  isCancelled,
  type AnalysisReport,
  type AudioTrack,
  type FileInfo,
  type NormalizeReport,
  type OutputMode,
  type Stage,
  type Targets,
} from "./api";

const VIDEO_EXTENSIONS = [
  "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "ts", "mts", "m2ts", "mpg", "mpeg", "3gp",
];

interface QueueItem {
  file: FileInfo;
  status: QueueStatus;
  report?: NormalizeReport;
  message?: string;
}

interface State {
  file: FileInfo | null;
  /** Several files chosen at once: they are normalized one after the other. */
  queue: QueueItem[];
  /** Which queue file is running, while a batch is. */
  batch: { index: number; total: number } | null;
  /** The user cancelled: the files still waiting are skipped. */
  stopped: boolean;
  analysis: AnalysisReport | null;
  /** Path the analysis belongs to; normalization reuses its measurement. */
  analyzedPath: string | null;
  /** Audio track the analysis measured. */
  analyzedTrack: number;
  busy: "analyze" | "normalize" | null;
  report: NormalizeReport | null;
  progress: { stage: Stage; percent: number } | null;
}

const state: State = { file: null, queue: [], batch: null, stopped: false, analysis: null, analyzedPath: null, analyzedTrack: 0, busy: null, report: null, progress: null };

function byId<T extends HTMLElement = HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) {
    throw new Error(`#${id} not found`);
  }
  return el as T;
}

const ui = {
  drop: byId("drop"),
  dropEmpty: document.querySelector<HTMLElement>(".drop-empty")!,
  dropFile: document.querySelector<HTMLElement>(".drop-file")!,
  fileName: byId("file-name"),
  fileDetail: byId("file-detail"),
  target: byId<HTMLSelectElement>("target"),
  ceiling: byId<HTMLSelectElement>("ceiling"),
  language: byId<HTMLSelectElement>("language"),
  analyze: byId<HTMLButtonElement>("analyze"),
  normalize: byId<HTMLButtonElement>("normalize"),
  normalizeLabel: byId("normalize-label"),
  output: byId<HTMLSelectElement>("output"),
  track: byId<HTMLSelectElement>("track"),
  targetCustom: byId<HTMLInputElement>("target-custom"),
  targetError: byId("target-error"),
  leveling: byId<HTMLInputElement>("leveling"),
  lossless: byId<HTMLInputElement>("lossless"),
  copyReport: byId<HTMLButtonElement>("copy-report"),
  resultChecks: byId("result-checks"),
  queue: byId("queue"),
  queueList: byId("queue-list"),
  queueSummary: byId("queue-summary"),
  queueAgain: byId<HTMLButtonElement>("queue-again"),
  trackField: byId("track-field"),
  cancel: byId<HTMLButtonElement>("cancel"),
  progress: byId("progress"),
  progressLabel: byId("progress-label"),
  progressPercent: byId("progress-percent"),
  progressFill: byId("progress-fill"),
  progressBar: document.querySelector<HTMLElement>("#progress .bar")!,
  error: byId("error"),
  errorText: byId("error-text"),
  actionHint: byId("action-hint"),
  again: byId<HTMLButtonElement>("again"),
  dialog: byId<HTMLDialogElement>("confirm-dialog"),
  confirmLead: byId("confirm-lead"),
  confirmTarget: byId("confirm-target"),
  confirmCeiling: byId("confirm-ceiling"),
  confirmGain: byId("confirm-gain"),
  confirmOk: byId<HTMLButtonElement>("confirm-ok"),
  confirmCancel: byId<HTMLButtonElement>("confirm-cancel"),
  analysis: byId("analysis"),
  result: byId("result"),
};

// ---------------------------------------------------------------- formatting

const lufs = (v: number) => `${decimal(v)} LUFS`;
const db = (v: number, unit = "dB") => `${v > 0.05 ? "+" : ""}${decimal(v)} ${unit}`;

function bytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${i === 0 ? n : decimal(n)} ${units[i]}`;
}

function duration(seconds: number): string {
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const pad = (x: number) => String(x).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

function channelsLabel(n: number): string {
  if (n === 1) {
    return t("channelsMono");
  }
  if (n === 2) {
    return t("channelsStereo");
  }
  if (n === 6) {
    return "5.1";
  }
  if (n === 8) {
    return "7.1";
  }
  return t("channelsCount", { count: n });
}

// ---------------------------------------------------------------- state -> UI

/** Which audio track to work on, and whether to normalize all of them. */
function trackChoice(): { track: number; all: boolean } {
  if (ui.track.value === "all") {
    return { track: 0, all: true };
  }
  return { track: Number(ui.track.value) || 0, all: false };
}

function trackLabel(track: AudioTrack): string {
  const parts = [track.codec.toUpperCase(), channelsLabel(track.channels)];
  if (track.language) {
    parts.push(track.language);
  }
  if (track.title) {
    parts.push(track.title);
  }
  return t("trackOption", { n: track.index + 1, details: parts.join(" · ") });
}

/** Rebuilds the track list, keeping the current choice when it still exists. */
function renderTracks() {
  const tracks = hasBatch() ? [] : (state.file?.audioTracks ?? []);
  const previous = ui.track.value;
  ui.trackField.hidden = tracks.length < 2;
  const options = tracks.map((track) => new Option(trackLabel(track), String(track.index)));
  if (tracks.length > 1) {
    options.push(new Option(t("trackAll"), "all"));
  }
  ui.track.replaceChildren(...options);
  ui.track.value = options.some((option) => option.value === previous) ? previous : "0";
}

function outputMode(): OutputMode {
  return ui.output.value === "copy" ? "copy" : "replace";
}

/** EBU R128 allows the programme loudness to be this far off the target. */
const LOUDNESS_TOLERANCE_LU = 0.5;
/** Loudness range above which leveling is worth suggesting. */
const WIDE_RANGE_LU = 15;
/** Same limits the engine enforces for the target. */
const TARGET_RANGE = { min: -40, max: -5 };

function targetLufs(): number {
  return ui.target.value === "custom" ? Number(ui.targetCustom.value) : Number(ui.target.value);
}

function targetValid(): boolean {
  if (ui.target.value === "custom" && ui.targetCustom.value === "") {
    return false;
  }
  const value = targetLufs();
  return Number.isFinite(value) && value >= TARGET_RANGE.min && value <= TARGET_RANGE.max;
}

function targets(): Targets {
  return { targetLufs: targetLufs(), truePeakDb: Number(ui.ceiling.value) };
}

function hasBatch(): boolean {
  return state.queue.length > 1;
}

function render() {
  const busy = state.busy !== null;
  const hasInput = state.file !== null || hasBatch();
  ui.dropEmpty.hidden = hasInput;
  ui.dropFile.hidden = !hasInput;
  ui.drop.classList.toggle("has-file", hasInput);
  ui.drop.setAttribute("aria-disabled", String(busy));
  if (hasBatch()) {
    const total = state.queue.reduce((sum, item) => sum + item.file.size, 0);
    ui.fileName.textContent = t("batchFiles", { count: state.queue.length });
    ui.fileName.title = state.queue.map((item) => item.file.path).join("\n");
    ui.fileDetail.textContent = bytes(total);
  } else if (state.file) {
    ui.fileName.textContent = state.file.name;
    ui.fileName.title = state.file.path;
    ui.fileDetail.textContent = `${bytes(state.file.size)} · ${state.file.directory}`;
  }

  const valid = targetValid();
  ui.targetCustom.hidden = ui.target.value !== "custom";
  ui.targetError.hidden = valid;
  ui.targetCustom.setAttribute("aria-invalid", String(!valid));
  ui.analyze.disabled = busy || !state.file || !valid;
  ui.normalize.disabled = busy || !hasInput || !valid;
  ui.normalizeLabel.textContent = t(outputMode() === "copy" ? "normalizeCopy" : "normalizeReplace");
  ui.output.disabled = busy;
  ui.track.disabled = busy;
  ui.targetCustom.disabled = busy;
  ui.leveling.disabled = busy;
  ui.lossless.disabled = busy;
  ui.target.disabled = busy;
  ui.ceiling.disabled = busy;
  ui.actionHint.hidden = hasInput;
  renderQueue();
  ui.cancel.hidden = !busy;
  ui.progress.hidden = !busy;
}

function stageLabel(stage: Stage): string {
  return t(`stage${stage[0].toUpperCase()}${stage.slice(1)}`);
}

function setProgress(stage: Stage, filePercent: number) {
  const batch = state.batch;
  const stageText = stageLabel(stage);
  const label = batch
    ? t("batchProgress", { current: batch.index + 1, total: batch.total, stage: stageText })
    : stageText;
  ui.progressLabel.textContent = label;
  const p = Math.max(0, Math.min(100, batch ? overallPercent(batch.index, batch.total, filePercent) : filePercent));
  state.progress = { stage, percent: filePercent };
  ui.progressPercent.textContent = `${Math.floor(p)}%`;
  ui.progressFill.style.transform = `scaleX(${p / 100})`;
  ui.progressBar.setAttribute("aria-valuenow", String(Math.floor(p)));
  ui.progressBar.setAttribute("aria-valuetext", `${label} ${Math.floor(p)}%`);
}

function showError(message: string | null) {
  ui.error.hidden = message === null;
  ui.errorText.textContent = message ?? "";
}

function scalePosition(value: number): string {
  const clamped = Math.max(-40, Math.min(0, value));
  return `${((clamped + 40) / 40) * 100}%`;
}

function renderAnalysis() {
  const report = state.analysis;
  ui.analysis.hidden = report === null;
  if (!report) {
    return;
  }

  const { media, measurement: m, assessment: a } = report;
  ui.analysis.dataset.verdict = a.verdict;
  byId("verdict-title").textContent = t(`verdict${a.verdict[0].toUpperCase()}${a.verdict.slice(1)}`);

  const off = Math.abs(a.deviationLu);
  let text: string;
  if (a.verdict === "none") {
    text = t("textOnTarget");
  } else if (off < 0.5) {
    text = t("textPeaks", { ceiling: decimal(a.truePeakCeilingDb) });
  } else {
    const direction = t(a.deviationLu < 0 ? "directionBelow" : "directionAbove");
    text = t("textOff", { off: decimal(off), direction, gain: db(a.gainDb) });
  }
  byId("verdict-text").textContent = text;

  byId("gain-value").textContent = db(a.gainDb);

  byId("scale-current").style.left = scalePosition(m.integratedLufs);
  byId("scale-target").style.left = scalePosition(a.targetLufs);

  byId("m-current").textContent = lufs(m.integratedLufs);
  byId("m-target").textContent = lufs(a.targetLufs);
  byId("m-peak").textContent = `${decimal(m.truePeakDb)} dBTP`;
  byId("m-lra").textContent = `${decimal(m.loudnessRange)} LU`;
  byId("m-audio").textContent =
    `${media.codec.toUpperCase()} · ${channelsLabel(media.channels)} · ${media.sampleRate / 1000} kHz · ${duration(media.duration)}`;

  const notes: string[] = [];
  if (a.limiterReductionDb > 0.3) {
    notes.push(t("noteLimiter", { db: decimal(a.limiterReductionDb) }));
  }
  if (a.gainCapped) {
    notes.push(t("noteGainCapped"));
  }
  if (m.loudnessRange > WIDE_RANGE_LU && !ui.leveling.checked) {
    notes.push(t("noteWideRange", { range: decimal(m.loudnessRange) }));
  }
  if (a.clipping) {
    notes.push(t("noteClipping"));
  }
  if (!media.hasVideo) {
    notes.push(t("noteNoVideo"));
  }
  const list = byId("notes");
  list.replaceChildren(
    ...notes.map((n) => {
      const li = document.createElement("li");
      li.textContent = n;
      return li;
    }),
  );
  list.hidden = notes.length === 0;
}

const CHECK_ICON = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><circle cx=\"12\" cy=\"12\" r=\"9\" /><path d=\"m8 12 3 3 5-6\" /></svg>";
const WARN_ICON = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"M12 4 3 20h18z\" /><path d=\"M12 10v4m0 3h.01\" /></svg>";

/** Did the result meet the loudness target and the peak ceiling? */
function renderChecks(report: NormalizeReport) {
  const deviation = Math.abs(report.outputLufs - report.targetLufs);
  const overPeak = report.outputTruePeakDb - report.truePeakCeilingDb;
  const checks = [
    deviation <= LOUDNESS_TOLERANCE_LU
      ? { ok: true, text: t("checkLoudnessOk", { tolerance: decimal(LOUDNESS_TOLERANCE_LU) }) }
      : { ok: false, text: t("checkLoudnessOff", { off: decimal(deviation) }) },
    overPeak <= 0
      ? { ok: true, text: t("checkPeakOk", { ceiling: decimal(report.truePeakCeilingDb) }) }
      : { ok: false, text: t("checkPeakOver", { over: decimal(overPeak) }) },
  ];
  ui.resultChecks.replaceChildren(
    ...checks.map((check) => {
      const item = document.createElement("li");
      item.className = check.ok ? "check ok" : "check bad";
      item.innerHTML = check.ok ? CHECK_ICON : WARN_ICON;
      const text = document.createElement("span");
      text.textContent = check.text;
      item.append(text);
      return item;
    }),
  );
}

/** Plain-text summary of a result, for pasting into a ticket or a message. */
function reportText(report: NormalizeReport): string {
  const name = report.path.split(/[/\\]/).pop() ?? report.path;
  const loudnessOk = Math.abs(report.outputLufs - report.targetLufs) <= LOUDNESS_TOLERANCE_LU;
  const peakOk = report.outputTruePeakDb <= report.truePeakCeilingDb;
  const yesNo = (ok: boolean) => t(ok ? "reportYes" : "reportNo");
  const levels =
    report.inputMaxShortTermLufs === undefined || report.inputMaxMomentaryLufs === undefined
      ? []
      : [
        t("reportMax", {
          short: lufs(report.inputMaxShortTermLufs),
          momentary: lufs(report.inputMaxMomentaryLufs),
        }),
      ];
  return [
    t("reportTitle"),
    t("reportFile", { name }),
    t("reportTargets", { target: lufs(report.targetLufs), ceiling: `${decimal(report.truePeakCeilingDb)} dBTP` }),
    t("reportBefore", {
      lufs: lufs(report.inputLufs),
      peak: `${decimal(report.inputTruePeakDb)} dBTP`,
      range: decimal(report.inputLoudnessRange),
    }),
    ...levels,
    t("reportAfter", { lufs: lufs(report.outputLufs), peak: `${decimal(report.outputTruePeakDb)} dBTP` }),
    t("reportGain", {
      gain: db(report.gainDb),
      limiter:
        report.limiterMaxReductionDb > 0.05
          ? t("limiterUpTo", { db: decimal(report.limiterMaxReductionDb) })
          : t("limiterIdle"),
    }),
    t("reportConformity", {
      tolerance: decimal(LOUDNESS_TOLERANCE_LU),
      loudness: yesNo(loudnessOk),
      peak: yesNo(peakOk),
    }),
    t("reportOutput", { path: report.outputPath, mode: t(report.replaced ? "reportReplaced" : "reportCopy") }),
  ].join("\n");
}

async function copyReport() {
  if (!state.report) {
    return;
  }
  try {
    await navigator.clipboard.writeText(reportText(state.report));
    ui.copyReport.textContent = t("copied");
    window.setTimeout(() => {
      ui.copyReport.textContent = t("copyReport");
    }, 2000);
  } catch {
    showError(t("copyFailed"));
  }
}

function renderResult(report: NormalizeReport | null) {
  state.report = report;
  ui.result.hidden = report === null;
  if (!report) {
    return;
  }
  const seconds = decimal(report.elapsedSeconds);
  byId("result-sub").textContent = report.replaced
    ? t("resultSub", { seconds })
    : t("resultSubCopy", { seconds, name: report.outputPath.split(/[/\\]/).pop() ?? report.outputPath });
  byId("result-sub").title = report.outputPath;
  const tracksNote = report.tracksProcessed > 1 ? ` ${t("resultTracks", { count: report.tracksProcessed })}` : "";
  const leveledNote = report.leveled ? ` ${t("resultLeveled")}` : "";
  const verdict = t("resultVerdict", { peak: decimal(report.outputTruePeakDb) }) + tracksNote + leveledNote;
  renderChecks(report);
  byId("result-verdict").textContent = report.outputMedia.hasVideo ? `${verdict} ${t("resultVerdictVideo")}` : verdict;
  byId("r-before").textContent = lufs(report.inputLufs);
  byId("r-after").textContent = lufs(report.outputLufs);
  byId("r-gain").textContent = db(report.gainDb);
  byId("r-peak").textContent = `${decimal(report.outputTruePeakDb)} dBTP`;
  byId("r-limiter").textContent =
    report.limiterMaxReductionDb > 0.05 ? t("limiterUpTo", { db: decimal(report.limiterMaxReductionDb) }) : t("limiterIdle");
  byId("r-size").textContent = `${bytes(report.sizeBefore)} → ${bytes(report.sizeAfter)}`;
  const chart = (id: string, value: number) => {
    byId(id).style.transform = `scaleX(${Math.max(2, Math.min(100, ((value + 60) / 60) * 100)) / 100})`;
  };
  chart("chart-loudness-before", report.inputLufs);
  chart("chart-loudness-after", report.outputLufs);
  chart("chart-peak-before", report.inputTruePeakDb);
  chart("chart-peak-after", report.outputTruePeakDb);
  byId("chart-loudness").textContent = `${lufs(report.inputLufs)} → ${lufs(report.outputLufs)}`;
  byId("chart-peak").textContent = `${db(report.inputTruePeakDb, "dBTP")} → ${db(report.outputTruePeakDb, "dBTP")}`;
  const before = report.inputMedia;
  const after = report.outputMedia;
  byId("r-codec").textContent = `${before.codec.toUpperCase()} → ${after.codec.toUpperCase()}`;
  byId("r-audio").textContent = `${channelsLabel(before.channels)} / ${before.sampleRate / 1000} kHz → ${channelsLabel(after.channels)} / ${after.sampleRate / 1000} kHz`;
  byId("r-duration").textContent = `${duration(before.duration)} → ${duration(after.duration)}`;
  byId("r-range").textContent = `${decimal(report.inputLoudnessRange)} LU`;
  const optionalLufs = (value: number | undefined) => (value === undefined ? "—" : lufs(value));
  byId("r-short").textContent = optionalLufs(report.inputMaxShortTermLufs);
  byId("r-momentary").textContent = optionalLufs(report.inputMaxMomentaryLufs);
  const videoLabel = (hasVideo: boolean) => t(hasVideo ? "videoKept" : "videoNone");
  byId("r-video").textContent = `${videoLabel(before.hasVideo)} → ${videoLabel(after.hasVideo)}`;
}

// ---------------------------------------------------------------- actions

async function selectFile(path: string) {
  if (state.busy) {
    return;
  }
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  try {
    const info = await api.inspectFile(path);
    state.file = info;
    state.queue = [];
    state.analysis = null;
    state.analyzedPath = null;
    ui.track.value = "0";
    showError(VIDEO_EXTENSIONS.includes(ext) ? null : t("errorNotVideo"));
    renderTracks();
    renderAnalysis();
    renderResult(null);
    render();
  } catch (err) {
    showError(errorMessage(err));
  }
}

/** One path behaves as before; several become a queue. */
async function selectFiles(paths: string[]) {
  if (state.busy || paths.length === 0) {
    return;
  }
  if (paths.length === 1) {
    await selectFile(paths[0]);
    return;
  }
  const results = await Promise.allSettled(paths.map((path) => api.inspectFile(path)));
  const files = results.flatMap((result) => (result.status === "fulfilled" ? [result.value] : []));
  if (files.length < 2) {
    const failure = results.find((result) => result.status === "rejected");
    if (files.length === 1) {
      await selectFile(files[0].path);
    } else {
      showError(failure?.status === "rejected" ? errorMessage(failure.reason) : null);
    }
    return;
  }
  state.queue = files.map((file) => ({ file, status: "waiting" as const }));
  state.file = null;
  state.analysis = null;
  state.analyzedPath = null;
  showError(null);
  renderTracks();
  renderAnalysis();
  renderResult(null);
  render();
}

async function pickFile() {
  if (state.busy) {
    return;
  }
  const selected = await open({
    multiple: true,
    directory: false,
    title: t("openTitle"),
    filters: [
      { name: t("openVideos"), extensions: VIDEO_EXTENSIONS },
      { name: t("openAll"), extensions: ["*"] },
    ],
  });
  const paths = Array.isArray(selected) ? selected : typeof selected === "string" ? [selected] : [];
  await selectFiles(paths);
}

async function runAnalysis() {
  if (!state.file || state.busy) {
    return;
  }
  const file = state.file;
  state.busy = "analyze";
  showError(null);
  renderResult(null);
  setProgress("analyze", 0);
  render();
  try {
    const { track } = trackChoice();
    state.analysis = await api.analyze(file.path, targets(), track);
    state.analyzedPath = file.path;
    state.analyzedTrack = track;
  } catch (err) {
    if (!isCancelled(err)) {
      showError(errorMessage(err));
    }
  } finally {
    state.busy = null;
    renderAnalysis();
    render();
  }
}

/** Asks before replacing: `lead` says what is replaced, `gain` what will be applied. */
function askReplace(lead: string, gain: string): Promise<boolean> {
  const goal = targets();
  ui.confirmLead.textContent = lead;
  ui.confirmTarget.textContent = lufs(goal.targetLufs);
  ui.confirmCeiling.textContent = `${decimal(goal.truePeakDb)} dBTP`;
  ui.confirmGain.textContent = gain;
  return new Promise((resolve) => {
    ui.dialog.returnValue = "";
    ui.dialog.addEventListener("close", () => resolve(ui.dialog.returnValue === "ok"), { once: true });
    ui.dialog.showModal();
    ui.confirmCancel.focus();
  });
}

async function runNormalize() {
  if (!state.file || state.busy) {
    return;
  }
  const file = state.file;
  const output = outputMode();
  // Only replacing the original needs a confirmation; a copy leaves it intact.
  let approved = output === "copy";
  if (!approved) {
    try {
      const known = state.analysis && state.analyzedPath === file.path ? state.analysis : null;
      approved = await askReplace(
        t("confirmMessage", { name: file.name }),
        known ? db(known.assessment.gainDb) : t("confirmGainPending"),
      );
    } catch (err) {
      showError(errorMessage(err));
      return;
    }
  }
  if (!approved || state.busy || state.file?.path !== file.path) {
    return;
  }
  const choice = trackChoice();
  const measured =
    state.analysis && state.analyzedPath === file.path && state.analyzedTrack === choice.track
      ? state.analysis.measurement
      : null;

  state.busy = "normalize";
  showError(null);
  renderResult(null);
  setProgress(measured === null ? "analyze" : "normalize", 0);
  render();
  try {
    const report = await api.normalize(file.path, targets(), measured, {
      output,
      track: choice.track,
      allTracks: choice.all,
      leveling: ui.leveling.checked,
      lossless: ui.lossless.checked,
    });
    if (report.replaced) {
      // The file on disk changed: old analysis no longer applies.
      state.analysis = null;
      state.analyzedPath = null;
      state.file = await api.inspectFile(file.path).catch(() => file);
      renderAnalysis();
    }
    renderResult(report);
  } catch (err) {
    showError(isCancelled(err) ? t("errorCancelled") : errorMessage(err));
  } finally {
    state.busy = null;
    render();
  }
}

async function reassess() {
  if (!state.analysis || state.busy) {
    return;
  }
  try {
    state.analysis = { ...state.analysis, assessment: await api.assess(state.analysis.measurement, targets()) };
    renderAnalysis();
  } catch (err) {
    showError(errorMessage(err));
  }
}

const QUEUE_ICONS: Record<QueueStatus, string> = {
  waiting: "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><circle cx=\"12\" cy=\"12\" r=\"9\" /></svg>",
  running: "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"M3 12h3l3-7 4 14 3-7h5\" /></svg>",
  done: "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><circle cx=\"12\" cy=\"12\" r=\"9\" /><path d=\"m8 12 3 3 5-6\" /></svg>",
  failed: "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><circle cx=\"12\" cy=\"12\" r=\"9\" /><path d=\"M12 8v5m0 3h.01\" /></svg>",
  skipped: "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><circle cx=\"12\" cy=\"12\" r=\"9\" /><path d=\"M8 12h8\" /></svg>",
};

function queueStatusText(item: QueueItem): string {
  switch (item.status) {
    case "waiting":
      return t("queueWaiting");
    case "running":
      return t("queueRunning");
    case "skipped":
      return t("queueSkipped");
    case "failed":
      return item.message ?? t("queueFailed");
    default:
      return item.report ? `${lufs(item.report.outputLufs)} · ${decimal(item.report.outputTruePeakDb)} dBTP` : t("queueDone");
  }
}

function renderQueue() {
  ui.queue.hidden = !hasBatch();
  if (!hasBatch()) {
    return;
  }
  ui.queueList.replaceChildren(
    ...state.queue.map((item) => {
      const row = document.createElement("li");
      row.className = `queue-item ${item.status}`;
      row.innerHTML = QUEUE_ICONS[item.status];
      const name = document.createElement("span");
      name.className = "queue-name";
      name.textContent = item.file.name;
      name.title = item.file.path;
      const status = document.createElement("span");
      status.className = "queue-status num";
      status.textContent = queueStatusText(item);
      row.append(name, status);
      return row;
    }),
  );
  const statuses = state.queue.map((item) => item.status);
  const summary = summarize(statuses);
  const started = statuses.some((status) => status !== "waiting");
  const finished = started && isFinished(statuses);
  let text = t("queueCount", { count: summary.total });
  if (finished) {
    text = t("batchSummary", { done: summary.done, total: summary.total });
    if (summary.failed > 0) {
      text += ` ${t("batchFailed", { failed: summary.failed })}`;
    }
    if (summary.skipped > 0) {
      text += ` ${t("batchSkipped", { skipped: summary.skipped })}`;
    }
  }
  ui.queueSummary.textContent = text;
  ui.queueAgain.hidden = !finished;
}

/** Normalizes every queued file in turn with the settings on screen. */
async function runBatch() {
  if (state.busy || !hasBatch()) {
    return;
  }
  const output = outputMode();
  let approved = output === "copy";
  if (!approved) {
    try {
      approved = await askReplace(t("confirmBatchMessage", { count: state.queue.length }), t("confirmGainEach"));
    } catch (err) {
      showError(errorMessage(err));
      return;
    }
  }
  if (!approved || state.busy) {
    return;
  }
  const items = state.queue;
  for (const item of items) {
    item.status = "waiting";
    item.report = undefined;
    item.message = undefined;
  }
  state.busy = "normalize";
  state.stopped = false;
  showError(null);
  render();
  for (const [index, item] of items.entries()) {
    if (state.stopped) {
      item.status = "skipped";
      continue;
    }
    item.status = "running";
    state.batch = { index, total: items.length };
    setProgress("analyze", 0);
    renderQueue();
    try {
      item.report = await api.normalize(item.file.path, targets(), null, {
        output,
        track: 0,
        allTracks: false,
        leveling: ui.leveling.checked,
        lossless: ui.lossless.checked,
      });
      item.status = "done";
    } catch (err) {
      if (isCancelled(err)) {
        item.status = "skipped";
        state.stopped = true;
      } else {
        item.status = "failed";
        item.message = errorMessage(err);
      }
    }
  }
  state.batch = null;
  state.busy = null;
  render();
}

async function checkForUpdates() {
  try {
    const update = await check();
    if (!update) {
      return;
    }
    const approved = await confirm(t("updateMessage", { version: update.version }), {
      title: t("updateTitle"),
      kind: "info",
      okLabel: t("updateOk"),
      cancelLabel: t("updateLater"),
    });
    if (approved) {
      await update.downloadAndInstall();
    }
  } catch (error) {
    console.warn("Update check failed", error);
  }
}

/** Re-render text built from state; static markup is handled by applyLanguage. */
function refreshLanguage() {
  renderTracks();
  renderAnalysis();
  renderResult(state.report);
  render();
  if (state.busy && state.progress) {
    setProgress(state.progress.stage, state.progress.percent);
  }
}

// ---------------------------------------------------------------- wiring

ui.drop.addEventListener("click", () => void pickFile());
ui.drop.addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    void pickFile();
  }
});
ui.analyze.addEventListener("click", () => void runAnalysis());
ui.normalize.addEventListener("click", () => void (hasBatch() ? runBatch() : runNormalize()));
ui.confirmOk.addEventListener("click", () => ui.dialog.close("ok"));
ui.confirmCancel.addEventListener("click", () => ui.dialog.close("cancel"));
function startOver() {
  state.file = null;
  state.queue = [];
  state.analysis = null;
  state.analyzedPath = null;
  showError(null);
  renderAnalysis();
  renderResult(null);
  render();
  ui.drop.focus();
}

ui.again.addEventListener("click", startOver);
ui.copyReport.addEventListener("click", () => void copyReport());
ui.queueAgain.addEventListener("click", startOver);
ui.cancel.addEventListener("click", () => {
  state.stopped = true;
  ui.progressLabel.textContent = t("cancelling");
  void api.cancel();
});
ui.output.addEventListener("change", render);
ui.target.addEventListener("change", render);
ui.targetCustom.addEventListener("input", () => {
  render();
  if (targetValid()) {
    void reassess();
  }
});
ui.track.addEventListener("change", () => {
  // The analysis measured another track: it no longer applies.
  if (state.analysis && state.analyzedTrack !== trackChoice().track) {
    state.analysis = null;
    renderAnalysis();
  }
});
ui.target.addEventListener("change", () => void reassess());
ui.ceiling.addEventListener("change", () => void reassess());
ui.language.addEventListener("change", () => {
  applyLanguage(ui.language.value as Language);
  refreshLanguage();
});

void api.onProgress((event) => {
  if (state.busy) {
    setProgress(event.stage, event.percent);
  }
});

// Files dropped on the window arrive as native paths through Tauri.
void getCurrentWebview().onDragDropEvent((event) => {
  const payload = event.payload;
  if (payload.type === "enter" || payload.type === "over") {
    ui.drop.classList.toggle("dragging", state.busy === null);
  } else if (payload.type === "leave") {
    ui.drop.classList.remove("dragging");
  } else if (payload.type === "drop") {
    ui.drop.classList.remove("dragging");
    void selectFiles(payload.paths);
  }
});

ui.language.value = currentLanguage();
applyLanguage(currentLanguage());
render();
// Wait a moment so the update prompt does not cover the first screen.
window.setTimeout(() => void checkForUpdates(), 4000);
