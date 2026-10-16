// Scorpio dashboard helpers. Progressive enhancement only, every page works without JS.
(function () {
  "use strict";

  // Show timestamps in the viewer's local time zone.
  document.querySelectorAll("time[data-ts]").forEach(function (el) {
    var ts = Number(el.dataset.ts);
    if (!ts) return;
    var d = new Date(ts * 1000);
    el.title = el.textContent;
    el.textContent = d.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
  });

  // Ask before destructive form submissions.
  document.querySelectorAll("form[data-confirm]").forEach(function (form) {
    form.addEventListener("submit", function (e) {
      if (!window.confirm(form.dataset.confirm)) e.preventDefault();
    });
  });

  // Only one reset password popover open at a time.
  document.querySelectorAll("details.popover").forEach(function (d) {
    d.addEventListener("toggle", function () {
      if (!d.open) return;
      document.querySelectorAll("details.popover[open]").forEach(function (other) {
        if (other !== d) other.open = false;
      });
    });
  });

  // Chips insert their text into the field named by data-target (the greeting template by
  // default): at the cursor, or appended with a space when data-append is set.
  document.querySelectorAll("[data-insert]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var target = document.getElementById(btn.dataset.target || "tpl");
      if (!target) return;
      var token = btn.dataset.insert;
      if (btn.dataset.append) {
        var current = target.value.trim();
        target.value = current ? current + " " + token : token;
      } else {
        target.setRangeText(token, target.selectionStart, target.selectionEnd, "end");
      }
      target.focus();
      target.dispatchEvent(new Event("input", { bubbles: true }));
    });
  });

  // Time zone fields: offer the browser's zone, and prefill it where data-detect-tz is set
  // (new digest rules). Only zones in the field's list are used, the server knows those.
  var localTz = "";
  try {
    localTz = Intl.DateTimeFormat().resolvedOptions().timeZone || "";
  } catch (e) {
    localTz = "";
  }
  function knowsLocalTz(input) {
    if (!localTz || !input.list) return false;
    return Array.prototype.some.call(input.list.options, function (o) {
      return o.value === localTz;
    });
  }
  document.querySelectorAll("input[data-detect-tz]").forEach(function (input) {
    if (knowsLocalTz(input)) input.value = localTz;
  });
  document.querySelectorAll("[data-local-tz]").forEach(function (btn) {
    var target = document.getElementById(btn.dataset.localTz);
    if (!target || !knowsLocalTz(target)) return;
    btn.textContent = "Use my time zone (" + localTz + ")";
    function sync() {
      btn.hidden = target.value.trim() === localTz;
    }
    btn.addEventListener("click", function () {
      target.value = localTz;
      target.dispatchEvent(new Event("input", { bubbles: true }));
      target.focus();
    });
    target.addEventListener("input", sync);
    sync();
  });

  initGreetingEditor();

  function initGreetingEditor() {
    var tpl = document.getElementById("tpl");
    var preview = document.getElementById("preview");
    if (!tpl || !preview) return;

    var count = document.getElementById("tpl-count");
    var text = document.getElementById("preview-text");
    var embed = document.getElementById("preview-embed");
    var embedText = document.getElementById("preview-embed-text");
    var useEmbed = document.getElementById("use-embed");
    var color = document.getElementById("embed-color");

    var sample = {
      username: "newmember",
      display_name: "NewMember",
      user_id: "123456789012345678",
      server: preview.dataset.server || "Server",
      member_count: String(Number(preview.dataset.count || 0) + 1)
    };

    // Builds DOM nodes (never HTML strings) for a template. Supports {placeholders}
    // and **bold**, which is enough to judge how the message will look.
    function render(target, source) {
      target.textContent = "";
      var bold = null;
      var parts = source.split(/(\{[a-z_]+\})/g);
      parts.forEach(function (part) {
        var m = /^\{([a-z_]+)\}$/.exec(part);
        if (m && m[1] === "user") {
          var mention = document.createElement("span");
          mention.className = "mention";
          mention.textContent = "@" + sample.display_name;
          (bold || target).appendChild(mention);
          return;
        }
        var value = m && Object.prototype.hasOwnProperty.call(sample, m[1]) ? sample[m[1]] : part;
        var chunks = value.split("**");
        chunks.forEach(function (chunk, i) {
          if (i > 0) {
            if (bold) {
              target.appendChild(bold);
              bold = null;
            } else {
              bold = document.createElement("strong");
            }
          }
          if (chunk) (bold || target).appendChild(document.createTextNode(chunk));
        });
      });
      if (bold) target.appendChild(bold);
    }

    function update() {
      var value = tpl.value;
      if (count) count.textContent = String(value.length);
      var asEmbed = useEmbed && useEmbed.checked;
      if (asEmbed) {
        if (/\{user\}/.test(value)) render(text, "{user}");
        else text.textContent = "";
        render(embedText, value);
        embed.hidden = false;
        if (color) embed.style.borderLeftColor = color.value;
      } else {
        render(text, value);
        embed.hidden = true;
      }
    }

    [tpl, useEmbed, color].forEach(function (el) {
      if (!el) return;
      el.addEventListener("input", update);
      el.addEventListener("change", update);
    });
    update();
  }
})();
