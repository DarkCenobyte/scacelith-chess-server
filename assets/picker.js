// Puts the visitor's language first in the list (the first of the browser's languages that the
// reference is translated into).
(function () {
  "use strict";
  var items = Array.prototype.slice.call(document.querySelectorAll(".languages li"));
  var wanted = (navigator.languages || [navigator.language || "en"]).map(function (l) {
    return l.toLowerCase();
  });
  function score(code) {
    code = code.toLowerCase();
    for (var i = 0; i < wanted.length; i++) {
      var w = wanted[i];
      if (w === code) return i * 2;
      if (code === "zh-hant" && /^zh-(tw|hk|mo)/.test(w)) return i * 2;
      if (code === "zh-hans" && /^zh(-(cn|sg))?$/.test(w)) return i * 2;
      if (w.split("-")[0] === code.split("-")[0] && code.indexOf("zh") !== 0) return i * 2 + 1;
    }
    return Infinity;
  }
  var best = null;
  items.forEach(function (li) {
    var s = score(li.getAttribute("lang"));
    if (s !== Infinity && (best === null || s < best.s)) best = { li: li, s: s };
  });
  if (best) {
    best.li.classList.add("preferred");
    best.li.parentNode.insertBefore(best.li, best.li.parentNode.firstChild);
  }
})();
