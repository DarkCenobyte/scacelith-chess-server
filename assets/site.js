// Starts Redoc on the page's OpenAPI description with the settings of #site-config (written by
// tools/build.py), keeps the language picker of the top bar, and translates the few strings of
// Redoc's interface that its `labels` option does not cover.
(function () {
  "use strict";

  var config = JSON.parse(document.getElementById("site-config").textContent);
  var lang = config.lang;

  var sans = {
    ja: '"Hiragino Sans", "Yu Gothic UI", "Yu Gothic", Meiryo, "Noto Sans CJK JP", "Noto Sans JP", ',
    "zh-Hans": '"PingFang SC", "Microsoft YaHei", "Noto Sans CJK SC", "Noto Sans SC", ',
    "zh-Hant": '"PingFang TC", "Microsoft JhengHei", "Noto Sans CJK TC", "Noto Sans TC", ',
    ar: '"Segoe UI", "Noto Sans Arabic", "Geeza Pro", Tahoma, ',
  }[lang] || "";
  sans += 'system-ui, -apple-system, "Segoe UI", Roboto, "Noto Sans", "Helvetica Neue", Arial, sans-serif';
  var mono = 'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace';

  var options = config.redoc;
  options.theme = {
    colors: {
      primary: { main: "#8a5a1c" },
      text: { primary: "#2a2622" },
      http: { get: "#2f7d4a", post: "#1f5f99", put: "#8a5a1c", delete: "#a33a2e" },
    },
    typography: {
      fontFamily: sans,
      fontSize: "15px",
      lineHeight: "1.6em",
      headings: { fontFamily: sans, fontWeight: "600" },
      code: { fontFamily: mono, fontSize: "13px", color: "#7a3f12", backgroundColor: "rgba(138, 90, 28, 0.08)" },
      links: { color: "#8a5a1c", visited: "#8a5a1c", hover: "#5c3b10" },
    },
    sidebar: { backgroundColor: "#f6f2ea", width: "290px", textColor: "#2a2622" },
    rightPanel: { backgroundColor: "#2b2520" },
  };

  var root = document.getElementById("redoc");
  Redoc.init("openapi.yaml", options, root, function (error) {
    if (error) {
      root.textContent = String(error);
    }
  });

  // The language picker opens the same place of the page in the other language (the anchors are
  // the same in every language).
  var picker = document.getElementById("site-lang");
  picker.addEventListener("change", function () {
    window.location.href = "../" + picker.value + "/" + window.location.hash;
  });

  // Redoc's own strings outside `labels`: replaced where they appear, as Redoc renders sections
  // (only the text of a node changes, never the nodes themselves).
  var text = config.text;
  var words = config.words;
  if (!Object.keys(text).length) {
    return;
  }
  // Redoc's constraints: "[ 1 .. 254 ] characters", "<= 64 items", "= 6 characters".
  var constraint = /^((?:\[ \S+ \.\. \S+ \]|[<>]?= \S+) )(characters|items|properties)$/;
  function translated(value) {
    if (Object.prototype.hasOwnProperty.call(text, value)) {
      return text[value];
    }
    // The same with the spaces around it kept ("Expand all ").
    var padded = /^(\s*)(.*?)(\s*)$/.exec(value);
    if (padded[2] !== value && Object.prototype.hasOwnProperty.call(text, padded[2])) {
      return padded[1] + text[padded[2]] + padded[3];
    }
    var m = constraint.exec(value);
    if (m && Object.prototype.hasOwnProperty.call(words, m[2])) {
      return m[1] + words[m[2]];
    }
    return null;
  }
  function translateElement(el) {
    if (el.placeholder && Object.prototype.hasOwnProperty.call(text, el.placeholder)) {
      el.placeholder = text[el.placeholder];
    }
    var nodes = el.childNodes;
    if (!nodes.length) {
      return;
    }
    var whole = "";
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i].nodeType !== Node.TEXT_NODE) {
        whole = null;
        break;
      }
      whole += nodes[i].nodeValue;
    }
    var replacement = whole !== null && nodes.length > 1 ? translated(whole) : null;
    if (replacement !== null) {
      nodes[0].nodeValue = replacement;
      for (var j = 1; j < nodes.length; j++) {
        nodes[j].nodeValue = "";
      }
    }
  }
  function translate(scope) {
    var walker = document.createTreeWalker(scope, NodeFilter.SHOW_TEXT | NodeFilter.SHOW_ELEMENT);
    var node = scope;
    while (node) {
      if (node.nodeType === Node.TEXT_NODE) {
        var value = translated(node.nodeValue);
        if (value !== null) {
          node.nodeValue = value;
        }
      } else if (node.nodeType === Node.ELEMENT_NODE) {
        translateElement(node);
      }
      node = walker.nextNode();
    }
  }
  var pending = false;
  new MutationObserver(function () {
    if (pending) {
      return;
    }
    pending = true;
    window.requestAnimationFrame(function () {
      pending = false;
      translate(root);
    });
  }).observe(root, { childList: true, subtree: true, characterData: true });
})();
