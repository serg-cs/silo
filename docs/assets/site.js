// Enable enhanced navigation styles only after JavaScript is available.
document.documentElement.classList.add("js");

const menuButton = document.querySelector("[data-menu-button]");
const siteNavigation = document.querySelector("[data-site-navigation]");

if (menuButton && siteNavigation) {
  menuButton.addEventListener("click", () => {
    const isOpen = siteNavigation.classList.toggle("is-open");
    menuButton.setAttribute("aria-expanded", String(isOpen));
  });

  siteNavigation.addEventListener("click", (event) => {
    if (event.target.closest("a")) {
      siteNavigation.classList.remove("is-open");
      menuButton.setAttribute("aria-expanded", "false");
    }
  });
}

// Add copy controls without changing the no-JavaScript reading experience.
for (const block of document.querySelectorAll("pre[data-copy]")) {
  const wrapper = document.createElement("div");
  const button = document.createElement("button");

  wrapper.className = "code-block";
  block.before(wrapper);
  wrapper.append(block);

  button.className = "copy-button";
  button.type = "button";
  button.textContent = "Copy";
  button.setAttribute("aria-label", "Copy code to clipboard");
  wrapper.append(button);

  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(block.textContent);
      button.textContent = "Copied";
      window.setTimeout(() => {
        button.textContent = "Copy";
      }, 1600);
    } catch {
      button.textContent = "Select to copy";
    }
  });
}
