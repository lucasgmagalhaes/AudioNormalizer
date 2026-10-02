import { FluentBundle, FluentResource } from "@fluent/bundle";
import ptBR from "./locales/pt-BR.ftl?raw";
import en from "./locales/en.ftl?raw";
import es from "./locales/es.ftl?raw";

export const languages = ["pt-BR", "en", "es"] as const;
export type Language = (typeof languages)[number];

const resources: Record<Language, string> = { "pt-BR": ptBR, en, es };

const bundles = Object.fromEntries(
  languages.map((language) => {
    const bundle = new FluentBundle(language, { useIsolating: false });
    const errors = bundle.addResource(new FluentResource(resources[language]));
    if (errors.length) {
      throw new Error(`Invalid Fluent resource for ${language}`);
    }
    return [language, bundle];
  }),
) as Record<Language, FluentBundle>;

export type MessageArgs = Record<string, string | number>;

function format(language: Language, key: string, args?: MessageArgs): string {
  const id = key.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);
  const pattern = bundles[language].getMessage(id)?.value;
  return pattern ? bundles[language].formatPattern(pattern, args ?? null, []) : key;
}

export function currentLanguage(): Language {
  const stored = localStorage.getItem("audio-normalizer.language");
  return languages.includes(stored as Language) ? stored as Language : "pt-BR";
}

export function t(key: string, args?: MessageArgs): string {
  return format(currentLanguage(), key, args);
}

const numberFormats = new Map<Language, Intl.NumberFormat>();

/** One-decimal number in the active interface language. */
export function decimal(value: number): string {
  const language = currentLanguage();
  let formatter = numberFormats.get(language);
  if (!formatter) {
    formatter = new Intl.NumberFormat(language, { minimumFractionDigits: 1, maximumFractionDigits: 1 });
    numberFormats.set(language, formatter);
  }
  return formatter.format(value);
}

export function applyLanguage(language: Language) {
  localStorage.setItem("audio-normalizer.language", language);
  document.documentElement.lang = language;
  document.querySelectorAll<HTMLElement>("[data-i18n]").forEach((el) => {
    el.textContent = format(language, el.dataset.i18n!);
  });
  document.querySelectorAll<HTMLElement>("[data-i18n-title]").forEach((el) => {
    el.title = format(language, el.dataset.i18nTitle!);
  });
  document.querySelectorAll<HTMLElement>("[data-i18n-aria-label]").forEach((el) => {
    el.setAttribute("aria-label", format(language, el.dataset.i18nAriaLabel!));
  });
}
