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

// The hero's F8: same card the app shows, and it marks the buffer as a saved clip.
const buffer = document.getElementById("buffer");
const toast = document.getElementById("toast");
let toastTimer;
const saveClip = () => {
  toast.innerHTML = "<strong>Clip saved</strong><span>Counter-Strike 2 · 60s</span>";
  toast.classList.add("is-on");
  buffer.classList.add("is-saved");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.classList.remove("is-on");
    buffer.classList.remove("is-saved");
  }, 2600);
};
document.getElementById("try").addEventListener("click", saveClip);
addEventListener("keydown", (e) => {
  if (e.key === "F8" && !e.repeat && !e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey) {
    e.preventDefault();
    saveClip();
  }
});

// The games strip loops by carrying a second copy of its list. Without motion it stays a plain scroller.
const strip = document.getElementById("strip");
if (matchMedia("(prefers-reduced-motion: no-preference)").matches) {
  const row = strip.querySelector(".strip-row");
  [...row.children].forEach((li) => {
    const copy = li.cloneNode(true);
    copy.setAttribute("aria-hidden", "true");
    row.append(copy);
  });
  strip.classList.add("is-rolling");
}

// Fade sections in as they scroll into view.
const io = new IntersectionObserver(
  (entries) => entries.forEach((e) => e.isIntersecting && (e.target.classList.add("in"), io.unobserve(e.target))),
  { rootMargin: "0px 0px -10% 0px" }
);
document.querySelectorAll(".reveal").forEach((el) => io.observe(el));
