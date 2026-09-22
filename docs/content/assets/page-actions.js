// "Copy page" split button beside each page's h1.
//
// Every action works off the page's markdown twin, which hooks/agent_head.py
// advertises as <link rel="alternate" type="text/markdown">. A page without
// that link gets no button.
(function () {
  "use strict";

  const ICONS = {
    copy: '<path d="M16 1H4c-1.1 0-2 .9-2 2v14h2V3h12V1zm3 4H8c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h11c1.1 0 2-.9 2-2V7c0-1.1-.9-2-2-2zm0 16H8V7h11v14z"/>',
    check: '<path d="M9 16.17L4.83 12l-1.42 1.41L9 19 21 7l-1.41-1.41z"/>',
    chevron: '<path d="M7 10l5 5 5-5z"/>',
    link: '<path d="M3.9 12c0-1.71 1.39-3.1 3.1-3.1h4V7H7c-2.76 0-5 2.24-5 5s2.24 5 5 5h4v-1.9H7c-1.71 0-3.1-1.39-3.1-3.1zM8 13h8v-2H8v2zm9-6h-4v1.9h4c1.71 0 3.1 1.39 3.1 3.1s-1.39 3.1-3.1 3.1h-4V17h4c2.76 0 5-2.24 5-5s-2.24-5-5-5z"/>',
    markdown: '<path d="M22.27 19.385H1.73A1.73 1.73 0 0 1 0 17.655V6.345a1.73 1.73 0 0 1 1.73-1.73h20.54A1.73 1.73 0 0 1 24 6.345v11.308a1.73 1.73 0 0 1-1.73 1.731zM5.769 15.923v-4.5l2.308 2.885l2.307-2.885v4.5h2.308V8.078h-2.308l-2.307 2.885l-2.308-2.885H3.46v7.847zM21.232 12h-2.309V8.077h-2.307V12h-2.308l3.461 4.039z"/>',
    chatgpt: '<path d="M22.282 9.821a6 6 0 0 0-.516-4.91a6.05 6.05 0 0 0-6.51-2.9A6.065 6.065 0 0 0 4.981 4.18a6 6 0 0 0-3.998 2.9a6.05 6.05 0 0 0 .743 7.097a5.98 5.98 0 0 0 .51 4.911a6.05 6.05 0 0 0 6.515 2.9A6 6 0 0 0 13.26 24a6.06 6.06 0 0 0 5.772-4.206a6 6 0 0 0 3.997-2.9a6.06 6.06 0 0 0-.747-7.073M13.26 22.43a4.48 4.48 0 0 1-2.876-1.04l.141-.081l4.779-2.758a.8.8 0 0 0 .392-.681v-6.737l2.02 1.168a.07.07 0 0 1 .038.052v5.583a4.504 4.504 0 0 1-4.494 4.494M3.6 18.304a4.47 4.47 0 0 1-.535-3.014l.142.085l4.783 2.759a.77.77 0 0 0 .78 0l5.843-3.369v2.332a.08.08 0 0 1-.033.062L9.74 19.95a4.5 4.5 0 0 1-6.14-1.646M2.34 7.896a4.5 4.5 0 0 1 2.366-1.973V11.6a.77.77 0 0 0 .388.677l5.815 3.354l-2.02 1.168a.08.08 0 0 1-.071 0l-4.83-2.786A4.504 4.504 0 0 1 2.34 7.872zm16.597 3.855l-5.833-3.387L15.119 7.2a.08.08 0 0 1 .071 0l4.83 2.791a4.494 4.494 0 0 1-.676 8.105v-5.678a.79.79 0 0 0-.407-.667m2.01-3.023l-.141-.085l-4.774-2.782a.78.78 0 0 0-.785 0L9.409 9.23V6.897a.07.07 0 0 1 .028-.061l4.83-2.787a4.5 4.5 0 0 1 6.68 4.66zm-12.64 4.135l-2.02-1.164a.08.08 0 0 1-.038-.057V6.075a4.5 4.5 0 0 1 7.375-3.453l-.142.08L8.704 5.46a.8.8 0 0 0-.393.681zm1.097-2.365l2.602-1.5l2.607 1.5v2.999l-2.597 1.5l-2.607-1.5Z"/>',
    claude: '<path d="M17.304 3.541h-3.672l6.696 16.918H24Zm-10.608 0L0 20.459h3.744l1.37-3.553h7.005l1.369 3.553h3.744L10.536 3.541Zm-.371 10.223L8.616 7.82l2.291 5.945Z"/>',
    external: '<path d="M14 3v2h3.59l-9.83 9.83 1.41 1.41L19 6.41V10h2V3h-7z"/>',
  };

  const MENU = [
    { action: "copy-link", icon: "link", label: "Copy markdown link" },
    { action: "view", icon: "markdown", label: "View as markdown", external: true },
    { action: "chatgpt", icon: "chatgpt", label: "Open in ChatGPT", external: true },
    { action: "claude", icon: "claude", label: "Open in Claude", external: true },
  ];

  function svg(name, cls) {
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" class="${cls}" aria-hidden="true">${ICONS[name]}</svg>`;
  }

  function markdownUrl() {
    const link = document.querySelector('link[rel="alternate"][type="text/markdown"]');
    return link ? new URL(link.getAttribute("href"), window.location.href).href : null;
  }

  function toast(message) {
    document.querySelector(".page-actions-toast")?.remove();
    const el = document.createElement("div");
    el.className = "page-actions-toast";
    el.setAttribute("role", "status");
    el.textContent = message;
    document.body.appendChild(el);
    requestAnimationFrame(() => el.classList.add("show"));
    setTimeout(() => {
      el.classList.remove("show");
      setTimeout(() => el.remove(), 300);
    }, 2500);
  }

  async function fetchMarkdown(url) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`${url} returned ${response.status}`);
    return response.text();
  }

  // Safari drops the user gesture across an await, so a writeText() after the
  // fetch is refused there. A ClipboardItem backed by a promise is written
  // inside the gesture and resolves later.
  async function copyMarkdown(url) {
    if (window.ClipboardItem && navigator.clipboard.write) {
      const blob = fetchMarkdown(url).then((text) => new Blob([text], { type: "text/plain" }));
      await navigator.clipboard.write([new ClipboardItem({ "text/plain": blob })]);
    } else {
      await navigator.clipboard.writeText(await fetchMarkdown(url));
    }
  }

  function askAssistant(base, url) {
    const prompt = `Read ${url} so I can ask questions about it.`;
    window.open(base + encodeURIComponent(prompt), "_blank", "noopener");
  }

  function build(url) {
    const container = document.createElement("div");
    container.className = "page-actions";

    const copyButton = document.createElement("button");
    copyButton.type = "button";
    copyButton.className = "page-actions__copy";
    copyButton.title = "Copy this page as markdown";
    copyButton.innerHTML = `${svg("copy", "page-actions__icon")}<span>Copy page</span>`;

    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "page-actions__toggle";
    toggle.title = "More options";
    toggle.setAttribute("aria-label", "More page options");
    toggle.setAttribute("aria-haspopup", "menu");
    toggle.setAttribute("aria-expanded", "false");
    toggle.innerHTML = svg("chevron", "page-actions__chevron");

    const menu = document.createElement("div");
    menu.className = "page-actions__menu";
    menu.setAttribute("role", "menu");
    menu.hidden = true;
    menu.innerHTML = MENU.map(
      (item) => `
      <button type="button" role="menuitem" tabindex="-1" data-action="${item.action}">
        ${svg(item.icon, "page-actions__icon")}
        <span>${item.label}</span>
        ${item.external ? svg("external", "page-actions__external") : ""}
      </button>`,
    ).join("");

    const items = () => Array.from(menu.querySelectorAll("[role=menuitem]"));

    function setOpen(open) {
      menu.hidden = !open;
      toggle.setAttribute("aria-expanded", String(open));
      container.classList.toggle("page-actions--open", open);
      if (open) items()[0].focus();
    }

    copyButton.addEventListener("click", async () => {
      const icon = copyButton.querySelector(".page-actions__icon");
      try {
        await copyMarkdown(url);
        icon.outerHTML = svg("check", "page-actions__icon page-actions__icon--done");
        toast("Page copied as markdown");
        setTimeout(() => {
          copyButton.querySelector(".page-actions__icon").outerHTML = svg("copy", "page-actions__icon");
        }, 2000);
      } catch (error) {
        console.error("Copy page failed:", error);
        toast("Couldn't copy the page");
      }
    });

    toggle.addEventListener("click", () => setOpen(menu.hidden));

    menu.addEventListener("click", async (event) => {
      const item = event.target.closest("[data-action]");
      if (!item) return;
      setOpen(false);
      switch (item.dataset.action) {
        case "copy-link":
          try {
            await navigator.clipboard.writeText(url);
            toast("Link copied");
          } catch (error) {
            console.error("Copy link failed:", error);
            toast("Couldn't copy the link");
          }
          break;
        case "view":
          window.open(url, "_blank", "noopener");
          break;
        case "chatgpt":
          askAssistant("https://chatgpt.com/?hints=search&q=", url);
          break;
        case "claude":
          askAssistant("https://claude.ai/new?q=", url);
          break;
      }
    });

    menu.addEventListener("keydown", (event) => {
      const list = items();
      const index = list.indexOf(document.activeElement);
      const focus = (i) => list[(i + list.length) % list.length].focus();
      switch (event.key) {
        case "ArrowDown": focus(index + 1); break;
        case "ArrowUp": focus(index - 1); break;
        case "Home": focus(0); break;
        case "End": focus(list.length - 1); break;
        case "Escape": setOpen(false); toggle.focus(); break;
        default: return;
      }
      event.preventDefault();
    });

    document.addEventListener("click", (event) => {
      if (!container.contains(event.target)) setOpen(false);
    });

    container.append(copyButton, toggle, menu);
    return container;
  }

  function init() {
    const url = markdownUrl();
    const title = document.querySelector(".md-content h1");
    if (!url || !title || document.querySelector(".page-actions")) return;

    const header = document.createElement("div");
    header.className = "page-actions-header";
    title.before(header);
    header.append(title, build(url));
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
