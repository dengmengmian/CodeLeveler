// The semantic snapshot, shared by BOTH backends.
//
// CodeLeveler owns refs (§18): Rust decides the generation, this script only
// stamps the label the DOM carries. A ref is `<generation>e<n>` written into
// `data-leveler-ref`, so a ref from a superseded snapshot simply matches no
// element and the action fails as stale — it can never retarget a lookalike.
//
// Runs verbatim under CDP `Runtime.evaluate` and WebDriver `execute/sync`, so
// the model reads one format whichever browser is driving.
(function (generation) {
  var ATTR = 'data-leveler-ref';
  var SKIP = { SCRIPT: 1, STYLE: 1, NOSCRIPT: 1, HEAD: 1, META: 1, LINK: 1, TITLE: 1, TEMPLATE: 1 };

  var old = document.querySelectorAll('[' + ATTR + ']');
  for (var i = 0; i < old.length; i++) old[i].removeAttribute(ATTR);

  function visible(el) {
    var s = window.getComputedStyle(el);
    if (!s || s.display === 'none' || s.visibility === 'hidden' || s.opacity === '0') return false;
    if (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true') return false;
    var r = el.getBoundingClientRect();
    // A zero-box element with no laid-out children is not on the page.
    return r.width > 0 || r.height > 0 || el.getClientRects().length > 0;
  }

  // The subset of ARIA roles this snapshot names. Explicit `role=` wins.
  function roleOf(el) {
    var explicit = el.getAttribute('role');
    if (explicit) return explicit.trim().split(/\s+/)[0];
    var tag = el.tagName;
    switch (tag) {
      case 'A': return el.hasAttribute('href') ? 'link' : null;
      case 'BUTTON': return 'button';
      case 'SELECT': return el.multiple ? 'listbox' : 'combobox';
      case 'TEXTAREA': return 'textbox';
      case 'IMG': return 'image';
      case 'H1': case 'H2': case 'H3': case 'H4': case 'H5': case 'H6': return 'heading';
      case 'NAV': return 'navigation';
      case 'MAIN': return 'main';
      case 'FORM': return 'form';
      case 'TABLE': return 'table';
      case 'UL': case 'OL': return 'list';
      case 'LI': return 'listitem';
      case 'LABEL': return 'label';
      case 'SUMMARY': return 'button';
      case 'IFRAME': return 'iframe';
      case 'INPUT': {
        var t = (el.type || 'text').toLowerCase();
        if (t === 'checkbox') return 'checkbox';
        if (t === 'radio') return 'radio';
        if (t === 'submit' || t === 'button' || t === 'reset' || t === 'image') return 'button';
        if (t === 'range') return 'slider';
        if (t === 'hidden') return null;
        return 'textbox';
      }
      default: return null;
    }
  }

  function directText(el) {
    var out = '';
    for (var i = 0; i < el.childNodes.length; i++) {
      var n = el.childNodes[i];
      if (n.nodeType === 3) out += n.nodeValue;
    }
    return out.replace(/\s+/g, ' ').trim();
  }

  function labelFor(el) {
    if (el.id) {
      var l = document.querySelector('label[for="' + CSS.escape(el.id) + '"]');
      if (l) return l.textContent.replace(/\s+/g, ' ').trim();
    }
    var p = el.closest ? el.closest('label') : null;
    return p ? p.textContent.replace(/\s+/g, ' ').trim() : '';
  }

  // Accessible name, in specification precedence order as far as this snapshot
  // goes: aria-label, aria-labelledby, an associated <label>, alt, title,
  // placeholder, then the element's own text.
  function nameOf(el) {
    var aria = el.getAttribute('aria-label');
    if (aria && aria.trim()) return aria.trim();
    var by = el.getAttribute('aria-labelledby');
    if (by) {
      var parts = [];
      by.trim().split(/\s+/).forEach(function (id) {
        var t = document.getElementById(id);
        if (t) parts.push(t.textContent.replace(/\s+/g, ' ').trim());
      });
      if (parts.length) return parts.join(' ');
    }
    var tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
      var lbl = labelFor(el);
      if (lbl) return lbl;
      if (el.placeholder) return el.placeholder.trim();
      if (el.name) return el.name.trim();
    }
    if (tag === 'IMG') return (el.getAttribute('alt') || '').trim();
    var title = el.getAttribute('title');
    if (title && title.trim()) return title.trim();
    var text = (el.textContent || '').replace(/\s+/g, ' ').trim();
    return text.length <= 120 ? text : text.slice(0, 117) + '...';
  }

  function stateOf(el) {
    var bits = [];
    if (el.disabled) bits.push('disabled');
    if (el.checked) bits.push('checked');
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
      var v = el.value;
      if (v) bits.push('value="' + (v.length <= 60 ? v : v.slice(0, 57) + '...') + '"');
    }
    if (el.tagName === 'SELECT' && el.selectedOptions && el.selectedOptions.length) {
      bits.push('selected="' + el.selectedOptions[0].text.replace(/\s+/g, ' ').trim() + '"');
    }
    return bits.length ? ' [' + bits.join(' ') + ']' : '';
  }

  var lines = [];
  var total = 0;
  var n = 0;

  function walk(el, depth) {
    if (SKIP[el.tagName]) return;
    if (!visible(el)) return;
    var role = roleOf(el);
    var emitted = false;
    if (role) {
      var name = nameOf(el);
      total++;
      n++;
      var ref = generation + 'e' + n;
      el.setAttribute(ATTR, ref);
      var line = new Array(depth + 1).join('  ') + '[' + ref + '] ' + role;
      if (name) line += ' "' + name + '"';
      line += stateOf(el);
      lines.push(line);
      emitted = true;
    } else {
      // Text that belongs to no named role still tells the model whether the
      // page changed, so it is reported — without a ref, since it is not
      // something to act on.
      var t = directText(el);
      if (t) {
        total++;
        lines.push(new Array(depth + 1).join('  ') + 'text "' + (t.length <= 160 ? t : t.slice(0, 157) + '...') + '"');
        emitted = true;
      }
    }
    var kids = el.children;
    for (var i = 0; i < kids.length; i++) walk(kids[i], emitted ? depth + 1 : depth);
  }

  if (document.body) walk(document.body, 0);

  return {
    url: location.href,
    title: document.title || '',
    text: lines.join('\n'),
    nodes: n,
    total: total
  };
})(GENERATION)
