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
  document.querySelectorAll<HTMLElement>("[data-i18n-aria-label]").forEach((el) => { el.setAttribute("aria-label", format(language, el.dataset.i18nAriaLabel!)); });
}
