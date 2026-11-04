// Shared by every test page. Reports to the local server and keeps a heartbeat so the runner
// knows the tab is alive and can ask it to close. Passwords and card numbers are never sent.
(function () {
  var q = new URLSearchParams(location.search);
  var tab = q.get("_t") || "";
  try { if (tab) sessionStorage.setItem("ptr_tab", tab); else tab = sessionStorage.getItem("ptr_tab") || ""; } catch (e) {}
  var page = location.pathname.replace(/^\//, "").replace(/index\.html$/, "").replace(/\.html$/, "").replace(/\/$/, "");
  var base = document.title;
  function post(path, body) {
    return fetch(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), keepalive: true }).then(function (r) { return r.json(); }).catch(function () { return {}; });
  }
  window.ptrReport = function (status, data, title) {
    if (title !== false) document.title = (title || base) + " | PTR " + status;
    window.__ptrTitleSet = true;
    return post("/result", { page: page, status: status, data: data || {}, title: document.title, tab: tab });
  };
  // Original form pages set document.title = "SUBMITTED ..." themselves; leave that title alone.
  new MutationObserver(function () {
    if (/^SUBMITTED/.test(document.title) && !window.__ptrSubmitted) {
      window.__ptrSubmitted = true;
      var data = {}, txt = document.title.length > 9 ? document.title : ((document.getElementById("out") || {}).textContent || "");
      try { data = JSON.parse(txt.replace(/^SUBMITTED\s*/, "")); } catch (e) {}
      post("/result", { page: page, status: "submitted", data: data, title: document.title, tab: tab });
    }
  }).observe(document.querySelector("title"), { childList: true, characterData: true, subtree: true });
  if (!window.__ptrNoLoad) ptrReport("visited", {}, false);
  function beat() {
    post("/hb", { tab: tab }).then(function (r) { if (r && r.close) { window.close(); setTimeout(function () { document.title = "close this tab"; }, 500); } });
  }
  if (tab) { beat(); setInterval(beat, 1000); }
})();
