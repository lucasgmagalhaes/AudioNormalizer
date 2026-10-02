export const languages = ["pt-BR", "en", "es"] as const;
export type Language = (typeof languages)[number];

const messages: Record<Language, Record<string, string>> = {
  "pt-BR": { language: "Idioma", tagline: "Deixa o volume dos seus vídeos no nível certo (EBU R128).", selectVideo: "Selecionar vídeo", dropTitle: "Arraste um vídeo para cá", dropSub: "ou clique para selecionar — MP4, MKV, MOV, WEBM, AVI…", change: "Trocar", settings: "Configurações", target: "Alvo de loudness", ceiling: "Teto de pico", analyze: "Analisar", normalize: "Normalizar", cancel: "Cancelar", preAnalysis: "Pré-análise", potential: "potencial de melhoria", current: "Loudness atual", gain: "Ajuste de ganho", peak: "Pico (true peak)", range: "Faixa dinâmica (LRA)", audio: "Áudio", complete: "Normalização concluída", before: "Antes", after: "Depois", appliedGain: "Ganho aplicado", finalPeak: "Pico final", limiter: "Limitador", size: "Tamanho", footnote: "O resultado substitui o arquivo original. Vídeo, legendas e demais faixas são copiados sem recompressão." },
  en: { language: "Language", tagline: "Gets your video volume to the right level (EBU R128).", selectVideo: "Select video", dropTitle: "Drop a video here", dropSub: "or click to select — MP4, MKV, MOV, WEBM, AVI…", change: "Change", settings: "Settings", target: "Loudness target", ceiling: "Peak ceiling", analyze: "Analyze", normalize: "Normalize", cancel: "Cancel", preAnalysis: "Pre-analysis", potential: "improvement potential", current: "Current loudness", gain: "Gain adjustment", peak: "True peak", range: "Dynamic range (LRA)", audio: "Audio", complete: "Normalization complete", before: "Before", after: "After", appliedGain: "Applied gain", finalPeak: "Final peak", limiter: "Limiter", size: "Size", footnote: "The result replaces the original file. Video, subtitles, and other tracks are copied without recompression." },
  es: { language: "Idioma", tagline: "Deja el volumen de tus vídeos en el nivel correcto (EBU R128).", selectVideo: "Seleccionar vídeo", dropTitle: "Arrastra un vídeo aquí", dropSub: "o haz clic para seleccionar — MP4, MKV, MOV, WEBM, AVI…", change: "Cambiar", settings: "Configuración", target: "Objetivo de sonoridad", ceiling: "Límite de pico", analyze: "Analizar", normalize: "Normalizar", cancel: "Cancelar", preAnalysis: "Preanálisis", potential: "potencial de mejora", current: "Sonoridad actual", gain: "Ajuste de ganancia", peak: "Pico verdadero", range: "Rango dinámico (LRA)", audio: "Audio", complete: "Normalización completada", before: "Antes", after: "Después", appliedGain: "Ganancia aplicada", finalPeak: "Pico final", limiter: "Limitador", size: "Tamaño", footnote: "El resultado reemplaza el archivo original. El vídeo, los subtítulos y otras pistas se copian sin recompresión." },
};

const bundles: Record<Language, FluentBundle> = Object.fromEntries(
  languages.map((language) => {
    const source = Object.entries(messages[language])
      .map(([id, value]) => `${id.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)} = ${value}`)
      .join("\n");
    const bundle = new FluentBundle(language, { useIsolating: false });
    const errors = bundle.addResource(new FluentResource(source));
    if (errors.length) throw new Error(`Invalid Fluent resource for ${language}`);
    return [language, bundle];
  }),
) as Record<Language, FluentBundle>;

function format(language: Language, key: string): string {
  const id = key.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);
  const pattern = bundles[language].getMessage(id)?.value;
  return pattern ? bundles[language].formatPattern(pattern, null, []) : key;
}

export function currentLanguage(): Language {
  const stored = localStorage.getItem("audio-normalizer.language");
  return languages.includes(stored as Language) ? stored as Language : "pt-BR";
}

export function t(key: string): string { return format(currentLanguage(), key); }

export function applyLanguage(language: Language) {
  localStorage.setItem("audio-normalizer.language", language);
  document.documentElement.lang = language;
  document.querySelectorAll<HTMLElement>("[data-i18n]").forEach((el) => { el.textContent = format(language, el.dataset.i18n!); });
  document.querySelectorAll<HTMLElement>("[data-i18n-aria-label]").forEach((el) => { el.setAttribute("aria-label", format(language, el.dataset.i18nAriaLabel!); });
}
import { FluentBundle, FluentResource } from "@fluent/bundle";
