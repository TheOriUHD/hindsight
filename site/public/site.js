// Every outbound project link in one place, so the repo can move without hunting through HTML.
const LINKS = {
  repo: "https://github.com/TheOriUHD/hindsight",
  download: "https://github.com/TheOriUHD/hindsight/releases/latest",
  releases: "https://github.com/TheOriUHD/hindsight/releases",
  issues: "https://github.com/TheOriUHD/hindsight/issues",
  license: "https://github.com/TheOriUHD/hindsight/blob/main/LICENSE",
};
document.querySelectorAll("[data-link]").forEach((a) => {
  const href = LINKS[a.dataset.link];
  if (href) a.href = href;
});

// Header gets a hairline once the page scrolls under it.
const header = document.querySelector(".site-header");
const onScroll = () => header.classList.toggle("is-scrolled", scrollY > 8);
addEventListener("scroll", onScroll, { passive: true });
onScroll();

// Theme toggle: remembers an explicit choice, otherwise follows the system.
document.getElementById("theme").addEventListener("click", () => {
  const root = document.documentElement;
  const dark = root.dataset.theme ? root.dataset.theme === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
  root.dataset.theme = dark ? "light" : "dark";
  try { localStorage.setItem("theme", root.dataset.theme); } catch {}
});

// Product window tabs.
const NOTES = {
  library: "<strong>Library.</strong> Every clip in one folder, grouped by day and by game. Hover a thumbnail to scrub through it before you open it.",
  player: "<strong>Player.</strong> Drag along the filmstrip and the frame follows your cursor. Set in and out points, save a trimmed copy, or export to fit an upload limit.",
  settings: "<strong>Settings.</strong> Resolution, frame rate, quality, clip length and the hotkey. Changes apply immediately, and it tells you how much memory your choice can use.",
};
const tabs = [...document.querySelectorAll(".tab")];
tabs.forEach((tab) =>
  tab.addEventListener("click", () => {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute("aria-selected", on);
      document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
    });
    document.getElementById("tab-note").innerHTML = NOTES[tab.id.replace("tab-", "")];
  })
);

// Fade sections in as they scroll into view.
const io = new IntersectionObserver(
  (entries) => entries.forEach((e) => e.isIntersecting && (e.target.classList.add("in"), io.unobserve(e.target))),
  { rootMargin: "0px 0px -10% 0px" }
);
document.querySelectorAll(".reveal").forEach((el) => io.observe(el));
