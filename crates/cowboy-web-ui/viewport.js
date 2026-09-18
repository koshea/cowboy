  // Track the visual viewport so the app fits the area above the on-screen
  // keyboard (iOS Safari doesn't honor interactive-widget). Sets --app-height,
  // which .page/.center use for their height.
  (function () {
    var vv = window.visualViewport;
    function set() {
      var h = vv ? vv.height : window.innerHeight;
      document.documentElement.style.setProperty('--app-height', h + 'px');
    }
    if (vv) {
      vv.addEventListener('resize', set);
      vv.addEventListener('scroll', set);
    }
    window.addEventListener('resize', set);
    set();
  })();
