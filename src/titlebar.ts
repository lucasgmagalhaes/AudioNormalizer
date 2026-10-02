import { getCurrentWindow } from "@tauri-apps/api/window";

/** One row of a drop-down menu. */
export interface MenuItem {
  id: string;
  label: string;
  shortcut?: string;
  /** "radio" and "check" items show a mark when `checked`. */
  kind?: "item" | "radio" | "check";
  checked?: boolean;
  disabled?: boolean;
  run: () => void;
}

export type MenuEntry = MenuItem | { separator: true } | { heading: string };

export interface Menu {
  id: string;
  label: string;
  entries: MenuEntry[];
}

export interface WindowLabels {
  minimize: string;
  maximize: string;
  restore: string;
  close: string;
}

export interface TitlebarOptions {
  /** Called every time a menu opens, so labels and marks are current. */
  menus: () => Menu[];
  windowLabels: () => WindowLabels;
}

function isItem(entry: MenuEntry): entry is MenuItem {
  return "run" in entry;
}

function element<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (className) {
    el.className = className;
  }
  return el;
}

/**
 * Wires the custom title bar: a keyboard-accessible menu bar (WAI-ARIA menubar
 * pattern) and the minimize / maximize / close buttons that replace the
 * operating system's own. Returns `refresh`, to call when the language changes.
 */
export function initTitlebar(options: TitlebarOptions): { refresh: () => void } {
  const bar = document.getElementById("menubar") as HTMLElement;
  let menus: Menu[] = [];
  let openIndex: number | null = null;
  let focusIndex = 0;

  const triggers: HTMLButtonElement[] = [];
  const popups: HTMLElement[] = [];

  function rows(popup: HTMLElement): HTMLElement[] {
    return [...popup.querySelectorAll<HTMLElement>('[role^="menuitem"]:not([aria-disabled="true"])')];
  }

  function close(restoreFocus = false) {
    if (openIndex === null) {
      return;
    }
    const index = openIndex;
    openIndex = null;
    popups[index].hidden = true;
    triggers[index].setAttribute("aria-expanded", "false");
    if (restoreFocus) {
      triggers[index].focus();
    }
  }

  function fill(index: number) {
    const popup = popups[index];
    popup.replaceChildren();
    for (const entry of menus[index].entries) {
      if ("separator" in entry) {
        const rule = element("div", "menu-separator");
        rule.setAttribute("role", "separator");
        popup.append(rule);
        continue;
      }
      if (!isItem(entry)) {
        const heading = element("div", "menu-heading");
        heading.setAttribute("role", "presentation");
        heading.textContent = entry.heading;
        popup.append(heading);
        continue;
      }
      const row = element("button", "menu-item");
      row.type = "button";
      const kind = entry.kind ?? "item";
      row.setAttribute("role", kind === "radio" ? "menuitemradio" : kind === "check" ? "menuitemcheckbox" : "menuitem");
      if (kind !== "item") {
        row.setAttribute("aria-checked", String(Boolean(entry.checked)));
      }
      if (entry.disabled) {
        row.setAttribute("aria-disabled", "true");
      }
      row.tabIndex = -1;
      const mark = element("span", "menu-mark");
      mark.setAttribute("aria-hidden", "true");
      const label = element("span", "menu-label");
      label.textContent = entry.label;
      row.append(mark, label);
      if (entry.shortcut) {
        const shortcut = element("span", "menu-shortcut");
        shortcut.textContent = entry.shortcut;
        row.append(shortcut);
      }
      row.addEventListener("click", () => {
        if (entry.disabled) {
          return;
        }
        close(true);
        entry.run();
      });
      popup.append(row);
    }
  }

  function open(index: number, focusFirst: boolean) {
    close();
    menus = options.menus();
    fill(index);
    openIndex = index;
    focusIndex = index;
    popups[index].hidden = false;
    triggers[index].setAttribute("aria-expanded", "true");
    if (focusFirst) {
      rows(popups[index])[0]?.focus();
    }
  }

  function moveTrigger(index: number, wasOpen: boolean) {
    const next = (index + triggers.length) % triggers.length;
    focusIndex = next;
    triggers.forEach((trigger, i) => (trigger.tabIndex = i === next ? 0 : -1));
    if (wasOpen) {
      open(next, true);
    } else {
      triggers[next].focus();
    }
  }

  function build() {
    menus = options.menus();
    triggers.length = 0;
    popups.length = 0;
    const roots = menus.map((menu, index) => {
      const root = element("div", "menu-root");
      const trigger = element("button", "menu-trigger");
      trigger.type = "button";
      trigger.id = `menu-${menu.id}`;
      trigger.textContent = menu.label;
      trigger.tabIndex = index === focusIndex ? 0 : -1;
      trigger.setAttribute("role", "menuitem");
      trigger.setAttribute("aria-haspopup", "menu");
      trigger.setAttribute("aria-expanded", "false");
      const popup = element("div", "menu");
      popup.setAttribute("role", "menu");
      popup.setAttribute("aria-labelledby", trigger.id);
      popup.hidden = true;

      trigger.addEventListener("click", () => (openIndex === index ? close() : open(index, false)));
      trigger.addEventListener("pointerenter", () => {
        if (openIndex !== null && openIndex !== index) {
          open(index, false);
        }
      });
      trigger.addEventListener("keydown", (event) => {
        if (event.key === "ArrowRight") {
          event.preventDefault();
          moveTrigger(index + 1, openIndex !== null);
        } else if (event.key === "ArrowLeft") {
          event.preventDefault();
          moveTrigger(index - 1, openIndex !== null);
        } else if (event.key === "ArrowDown" || event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          open(index, true);
        } else if (event.key === "Escape") {
          close(true);
        }
      });
      popup.addEventListener("keydown", (event) => {
        const items = rows(popup);
        const at = items.indexOf(document.activeElement as HTMLElement);
        const go = (to: number) => items[(to + items.length) % items.length]?.focus();
        switch (event.key) {
          case "ArrowDown":
            event.preventDefault();
            go(at + 1);
            break;
          case "ArrowUp":
            event.preventDefault();
            go(at <= 0 ? items.length - 1 : at - 1);
            break;
          case "Home":
            event.preventDefault();
            go(0);
            break;
          case "End":
            event.preventDefault();
            go(items.length - 1);
            break;
          case "ArrowRight":
            event.preventDefault();
            moveTrigger(index + 1, true);
            break;
          case "ArrowLeft":
            event.preventDefault();
            moveTrigger(index - 1, true);
            break;
          case "Escape":
            event.preventDefault();
            close(true);
            break;
          case "Tab":
            close();
            break;
        }
      });

      triggers.push(trigger);
      popups.push(popup);
      root.append(trigger, popup);
      return root;
    });
    bar.replaceChildren(...roots);
  }

  document.addEventListener("pointerdown", (event) => {
    if (openIndex !== null && !bar.contains(event.target as Node)) {
      close();
    }
  });
  window.addEventListener("blur", () => close());

  // ---------------------------------------------------------- window buttons
  const buttons = {
    minimize: document.getElementById("win-minimize") as HTMLButtonElement,
    maximize: document.getElementById("win-maximize") as HTMLButtonElement,
    close: document.getElementById("win-close") as HTMLButtonElement,
  };
  let maximized = false;

  function labelButtons() {
    const labels = options.windowLabels();
    const maximizeLabel = maximized ? labels.restore : labels.maximize;
    buttons.minimize.title = buttons.minimize.ariaLabel = labels.minimize;
    buttons.maximize.title = buttons.maximize.ariaLabel = maximizeLabel;
    buttons.maximize.dataset.state = maximized ? "maximized" : "normal";
    buttons.close.title = buttons.close.ariaLabel = labels.close;
  }

  try {
    const current = getCurrentWindow();
    buttons.minimize.addEventListener("click", () => void current.minimize());
    buttons.maximize.addEventListener("click", () => void current.toggleMaximize());
    buttons.close.addEventListener("click", () => void current.close());
    const syncMaximized = async () => {
      maximized = await current.isMaximized();
      labelButtons();
    };
    void current.onResized(() => void syncMaximized());
    void syncMaximized();
  } catch {
    // Outside the desktop shell (plain browser) there is no window to control.
  }

  build();
  labelButtons();

  return {
    refresh() {
      close();
      build();
      labelButtons();
    },
  };
}
