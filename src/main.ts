import { getCurrentWebview } from "@tauri-apps/api/webview";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { check } from "@tauri-apps/plugin-updater";
import { applyLanguage, currentLanguage, type Language } from "./i18n";
import {
  api,
  errorMessage,
  isCancelled,
  type AnalysisReport,
  type FileInfo,
  type NormalizeReport,
  type Stage,
  type Targets,
  type Verdict,
} from "./api";

const VIDEO_EXTENSIONS = [
  "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "ts", "mts", "m2ts", "mpg", "mpeg", "3gp",
];

const STAGE_LABELS: Record<Stage, string> = {
  analyze: "Medindo o loudness do áudio…",
  calibrate: "Ajustando o limitador para atingir o alvo…",
  normalize: "Aplicando normalização…",
  retry: "Recodificando para conter picos do codificador…",
  verify: "Conferindo o arquivo gerado…",
  finalize: "Verificando e substituindo o arquivo…",
};

const VERDICTS: Record<Verdict, string> = {
  none: "Já está no nível ideal",
  small: "Dá para melhorar um pouco",
  moderate: "Melhoria considerável",
  large: "Melhoria grande",
};

interface State {
  file: FileInfo | null;
  analysis: AnalysisReport | null;
  /** Path the analysis belongs to; normalization reuses its measurement. */
  analyzedPath: string | null;
  busy: "analyze" | "normalize" | null;
}

const state: State = { file: null, analysis: null, analyzedPath: null, busy: null };

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
  cancel: byId<HTMLButtonElement>("cancel"),
  progress: byId("progress"),
  progressLabel: byId("progress-label"),
  progressPercent: byId("progress-percent"),
  progressFill: byId("progress-fill"),
  progressBar: document.querySelector<HTMLElement>("#progress .bar")!,
  error: byId("error"),
  analysis: byId("analysis"),
  result: byId("result"),
};

// ---------------------------------------------------------------- formatting

const nf1 = new Intl.NumberFormat("pt-BR", { minimumFractionDigits: 1, maximumFractionDigits: 1 });

const lufs = (v: number) => `${nf1.format(v)} LUFS`;
const db = (v: number, unit = "dB") => `${v > 0.05 ? "+" : ""}${nf1.format(v)} ${unit}`;

function bytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${i === 0 ? n : nf1.format(n)} ${units[i]}`;
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
    return "mono";
  }
  if (n === 2) {
    return "estéreo";
  }
  if (n === 6) {
    return "5.1";
  }
  if (n === 8) {
    return "7.1";
  }
  return `${n} canais`;
}

// ---------------------------------------------------------------- state -> UI

function targets(): Targets {
  return { targetLufs: Number(ui.target.value), truePeakDb: Number(ui.ceiling.value) };
}

function render() {
  const busy = state.busy !== null;
  ui.dropEmpty.hidden = state.file !== null;
  ui.dropFile.hidden = state.file === null;
  ui.drop.classList.toggle("has-file", state.file !== null);
  ui.drop.setAttribute("aria-disabled", String(busy));
  if (state.file) {
    ui.fileName.textContent = state.file.name;
    ui.fileName.title = state.file.path;
    ui.fileDetail.textContent = `${bytes(state.file.size)} · ${state.file.directory}`;
  }

  ui.analyze.disabled = busy || !state.file;
  ui.normalize.disabled = busy || !state.file;
  ui.target.disabled = busy;
  ui.ceiling.disabled = busy;
  ui.cancel.hidden = !busy;
  ui.progress.hidden = !busy;
}

function setProgress(stage: Stage, percent: number) {
  ui.progressLabel.textContent = STAGE_LABELS[stage];
  const p = Math.max(0, Math.min(100, percent));
  ui.progressPercent.textContent = `${Math.floor(p)}%`;
  ui.progressFill.style.width = `${p}%`;
  ui.progressBar.setAttribute("aria-valuenow", String(Math.floor(p)));
  ui.progressBar.setAttribute("aria-valuetext", `${STAGE_LABELS[stage]} ${Math.floor(p)}%`);
}

function showError(message: string | null) {
  ui.error.hidden = message === null;
  ui.error.textContent = message ?? "";
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
  byId("verdict-title").textContent = VERDICTS[a.verdict];

  const off = Math.abs(a.deviationLu);
  let text: string;
  if (a.verdict === "none") {
    text = "O volume já está dentro do alvo. Normalizar fará pouca diferença.";
  } else if (off < 0.5) {
    text = `O volume está no alvo, mas os picos passam do teto de ${nf1.format(a.truePeakCeilingDb)} dBTP.`;
  } else {
    const direction = a.deviationLu < 0 ? "abaixo" : "acima";
    text = `O áudio está ${nf1.format(off)} LU ${direction} do alvo; será aplicado ganho de ${db(a.gainDb)}.`;
  }
  byId("verdict-text").textContent = text;

  byId("potential-value").textContent = `${Math.round(a.improvementPercent)}%`;
  byId("potential-fill").style.width = `${Math.max(2, a.improvementPercent)}%`;

  byId("scale-current").style.left = scalePosition(m.integratedLufs);
  byId("scale-target").style.left = scalePosition(a.targetLufs);

  byId("m-current").textContent = lufs(m.integratedLufs);
  byId("m-target").textContent = lufs(a.targetLufs);
  byId("m-gain").textContent = db(a.gainDb);
  byId("m-peak").textContent = `${nf1.format(m.truePeakDb)} dBTP`;
  byId("m-lra").textContent = `${nf1.format(m.loudnessRange)} LU`;
  byId("m-audio").textContent =
    `${media.codec.toUpperCase()} · ${channelsLabel(media.channels)} · ${media.sampleRate / 1000} kHz · ${duration(media.duration)}`;

  const notes: string[] = [];
  if (a.limiterReductionDb > 0.3) {
    notes.push(
      `O limitador vai segurar os picos em até ${nf1.format(a.limiterReductionDb)} dB para não distorcer.`,
    );
  }
  if (a.gainCapped) {
    notes.push("O áudio é muito baixo: o ganho foi limitado a 24 dB para não amplificar ruído demais.");
  }
  if (a.clipping) {
    notes.push("O original já encosta em 0 dBFS — pode haver distorção (clipping) que a normalização não remove.");
  }
  if (!media.hasVideo) {
    notes.push("Nenhuma faixa de vídeo encontrada; apenas o áudio será processado.");
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

function renderResult(report: NormalizeReport | null) {
  ui.result.hidden = report === null;
  if (!report) {
    return;
  }
  byId("result-sub").textContent = `Arquivo substituído em ${nf1.format(report.elapsedSeconds)} s.`;
  byId("result-sub").title = report.path;
  byId("r-before").textContent = lufs(report.inputLufs);
  byId("r-after").textContent = lufs(report.outputLufs);
  byId("r-gain").textContent = db(report.gainDb);
  byId("r-peak").textContent = `${nf1.format(report.outputTruePeakDb)} dBTP`;
  byId("r-limiter").textContent =
    report.limiterMaxReductionDb > 0.05 ? `até ${nf1.format(report.limiterMaxReductionDb)} dB` : "não atuou";
  byId("r-size").textContent = `${bytes(report.sizeBefore)} → ${bytes(report.sizeAfter)}`;
  const chart = (id: string, value: number) => {
    byId(id).style.width = `${Math.max(2, Math.min(100, ((value + 60) / 60) * 100))}%`;
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
  byId("r-video").textContent = `${before.hasVideo ? "preservado" : "sem vídeo"} → ${after.hasVideo ? "preservado" : "sem vídeo"}`;
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
    state.analysis = null;
    state.analyzedPath = null;
    showError(VIDEO_EXTENSIONS.includes(ext) ? null : "Esse arquivo não parece ser um vídeo; vou tentar mesmo assim.");
    renderAnalysis();
    renderResult(null);
    render();
  } catch (err) {
    showError(errorMessage(err));
  }
}

async function pickFile() {
  if (state.busy) {
    return;
  }
  const selected = await open({
    multiple: false,
    directory: false,
    title: "Selecione um vídeo",
    filters: [
      { name: "Vídeos", extensions: VIDEO_EXTENSIONS },
      { name: "Todos os arquivos", extensions: ["*"] },
    ],
  });
  if (typeof selected === "string") {
    await selectFile(selected);
  }
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
    state.analysis = await api.analyze(file.path, targets());
    state.analyzedPath = file.path;
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

async function runNormalize() {
  if (!state.file || state.busy) {
    return;
  }
  const file = state.file;
  let approved: boolean;
  try {
    approved = await confirm(
      `“${file.name}” será substituído pelo vídeo normalizado. Essa ação não pode ser desfeita.`,
      { title: "Substituir vídeo original?", kind: "warning", okLabel: "Normalizar e substituir", cancelLabel: "Cancelar" },
    );
  } catch (err) {
    showError(errorMessage(err));
    return;
  }
  if (!approved || state.busy || state.file?.path !== file.path) {
    return;
  }
  const measured =
    state.analysis && state.analyzedPath === file.path ? state.analysis.measurement : null;

  state.busy = "normalize";
  showError(null);
  renderResult(null);
  setProgress(measured === null ? "analyze" : "normalize", 0);
  render();
  try {
    const report = await api.normalize(file.path, targets(), measured);
    // The file on disk changed: old analysis no longer applies.
    state.analysis = null;
    state.analyzedPath = null;
    state.file = await api.inspectFile(file.path).catch(() => file);
    renderAnalysis();
    renderResult(report);
  } catch (err) {
    showError(isCancelled(err) ? "Processamento cancelado. O arquivo original não foi alterado." : errorMessage(err));
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

async function checkForUpdates() {
  try {
    const update = await check();
    if (!update) {
      return;
    }
    const approved = await confirm(`Uma atualização (${update.version}) está disponível. Instalar agora?`, {
      title: "Atualização disponível",
      kind: "info",
      okLabel: "Instalar e reiniciar",
      cancelLabel: "Depois",
    });
    if (approved) {
      await update.downloadAndInstall();
    }
  } catch (error) {
    console.warn("Update check failed", error);
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
ui.normalize.addEventListener("click", () => void runNormalize());
ui.cancel.addEventListener("click", () => {
  ui.progressLabel.textContent = "Cancelando…";
  void api.cancel();
});
ui.target.addEventListener("change", () => void reassess());
ui.ceiling.addEventListener("change", () => void reassess());
ui.language.addEventListener("change", () => applyLanguage(ui.language.value as Language));

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
    const [first] = payload.paths;
    if (first) {
      void selectFile(first);
    }
  }
});

ui.language.value = currentLanguage();
applyLanguage(currentLanguage());
render();
void checkForUpdates();
