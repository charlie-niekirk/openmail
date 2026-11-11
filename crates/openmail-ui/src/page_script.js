// Openmail's one trusted script, injected by the app into the reading
// pane's webview at document start (a user script, which the page's CSP
// doesn't govern). Email JavaScript stays off: the webview runs no script
// that comes with the content.
//
// It keeps what the reader did with the conversation across document
// reloads (showing images, a new message arriving, switching light and
// dark): which sections are open, and where the document is scrolled. It
// reports that state to the app as it changes, and the app hands it back
// in the next document's `<meta name="om-state">`, which this script
// applies before the page is first painted.
(function () {
  "use strict";
  if (window.__openmail) return;
  Object.defineProperty(window, "__openmail", { value: true });

  var SECTIONS = "details.om-message[id],details.om-quote[id]";
  var MESSAGES = "details.om-message[id]";
  // How long after loading the document keeps its place while its
  // content (images) settles, unless the reader scrolls first.
  var SETTLE_MS = 10000;

  var generation = 0;
  var timer = 0;
  // While set, layout changes keep the restored place.
  var anchor = null;

  function post(message) {
    try {
      window.ipc.postMessage(JSON.stringify(message));
    } catch (_) {}
  }

  // The first message section still in view, and how far the viewport's
  // top is into it.
  function currentAnchor() {
    var messages = document.querySelectorAll(MESSAGES);
    for (var i = 0; i < messages.length; i++) {
      var rect = messages[i].getBoundingClientRect();
      if (rect.bottom > 0) return { id: messages[i].id, offset: -rect.top };
    }
    return null;
  }

  function report() {
    timer = 0;
    var open = {};
    var sections = document.querySelectorAll(SECTIONS);
    for (var i = 0; i < sections.length; i++) open[sections[i].id] = sections[i].open;
    var place = currentAnchor();
    post({
      generation: generation,
      open: open,
      anchor: place && place.id,
      offset: place ? place.offset : 0,
      x: window.scrollX,
      y: window.scrollY
    });
  }

  function schedule() {
    if (!timer) timer = setTimeout(report, 120);
  }

  function scrollToAnchor() {
    if (!anchor) return;
    var section = anchor.id && document.getElementById(anchor.id);
    var y = anchor.y;
    if (section && section.matches(MESSAGES)) {
      y = section.getBoundingClientRect().top + window.scrollY + anchor.offset;
    }
    window.scrollTo(anchor.x, y);
  }

  function release() {
    anchor = null;
  }

  function restore(state) {
    var open = state.open || {};
    var sections = document.querySelectorAll(SECTIONS);
    for (var i = 0; i < sections.length; i++) {
      var id = sections[i].id;
      if (Object.prototype.hasOwnProperty.call(open, id)) sections[i].open = !!open[id];
    }
    if (typeof state.y !== "number") return;
    anchor = {
      id: state.anchor,
      offset: +state.offset || 0,
      x: +state.x || 0,
      y: +state.y || 0
    };
    scrollToAnchor();
    // Keep the place while images and the like change the layout, until
    // the reader takes over.
    var thread = document.querySelector("main.om-thread");
    if (thread && window.ResizeObserver) {
      var observer = new ResizeObserver(scrollToAnchor);
      observer.observe(thread);
      setTimeout(function () {
        observer.disconnect();
        release();
      }, SETTLE_MS);
    }
    window.addEventListener("load", scrollToAnchor);
    ["wheel", "mousedown", "keydown", "touchstart"].forEach(function (type) {
      window.addEventListener(type, release, { capture: true, passive: true });
    });
  }

  document.addEventListener("DOMContentLoaded", function () {
    var meta = document.head && document.head.querySelector('meta[name="om-state"]');
    var state = null;
    try {
      state = meta && JSON.parse(meta.content);
    } catch (_) {}
    if (state && typeof state === "object") {
      generation = +state.generation || 0;
      restore(state);
    }
    report();
  });
  // `toggle` doesn't bubble, so listen while it is captured.
  document.addEventListener("toggle", schedule, true);
  window.addEventListener("scroll", schedule, { passive: true });
})();
