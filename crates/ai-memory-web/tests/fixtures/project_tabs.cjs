// Run the shipped tab controller against the rendered links and panel states.
const assert = require('node:assert/strict');
const vm = require('node:vm');
const html = require('node:fs').readFileSync(0, 'utf8');
const script = html.match(/<script id="project-tabs-behavior">([\s\S]*?)<\/script>/)[1];
const names = ['overview', 'agents', 'tokens', 'sessions', 'knowledge'];
const attrs = tag => Object.fromEntries(Array.from(tag.matchAll(/([\w-]+)="([^"]*)"/g), m => [m[1], m[2]]));

function browser(initial = 'https://memory.test/w/team/app') {
  const location = {href: initial}, nodes = new Map(), events = new Map();
  const document = {activeElement: {closest: () => null}, getElementById: id => nodes.get(id) || null};
  const node = attributes => ({
    attributes, events: {}, dataset: {}, hidden: false,
    getAttribute(key) {return this.attributes[key];},
    setAttribute(key, value) {this.attributes[key] = value;},
    closest() {return this.panel || null;},
    addEventListener(key, callback) {this.events[key] = callback;},
    focus() {document.activeElement = this;},
  });
  for (const match of html.matchAll(/<section id="project-panel-[^>]+>/g)) {
    const attributes = attrs(match[0]), panel = node(attributes);
    panel.id = attributes.id; panel.panel = panel; panel.hidden = /\shidden\b/.test(match[0]);
    nodes.set(panel.id, panel);
  }
  const tabs = Array.from(html.matchAll(/<a id="project-tab-[^>]+>/g), match => {
    const attributes = attrs(match[0]), tab = node(attributes);
    tab.dataset.projectTab = attributes['data-project-tab'];
    tab.href = new URL(attributes.href, 'https://memory.test/').href;
    tab.tabIndex = Number(attributes.tabindex || 0);
    nodes.set(attributes.id, tab);return tab;
  });
  // An inner anchor exercises restoring a link into a hidden panel.
  nodes.set('project-search-q', {closest: () => nodes.get('project-panel-knowledge')});
  document.querySelector = () => ({querySelectorAll: () => tabs});
  const emit = key => (events.get(key) || []).forEach(fn => fn({}));
  const stack = [initial];let index = 0;
  const history = {
    pushState(_, __, url) {stack.splice(++index);stack.push(String(url));location.href = String(url);},
    replaceState(_, __, url) {stack[index] = location.href = String(url);},
    back() {if(index) {location.href = stack[--index];emit('popstate');}},
    forward() {if(index < stack.length - 1) {location.href = stack[++index];emit('popstate');}},
  };
  const window = {addEventListener(key, fn) {events.set(key, [...(events.get(key) || []), fn]);}};
  vm.runInNewContext(script, {document, window, location, history, URL});
  return {
    location, history, document, nodes, tabs, emit,
    click(name, extra = {}) {
      let prevented = false;
      tabs.find(t => t.dataset.projectTab === name).events.click({button: 0, preventDefault() {prevented = true;}, ...extra});
      return prevented;
    },
    key(key, extra = {}) {
      let prevented = false;
      document.activeElement.events.keydown({key, preventDefault() {prevented = true;}, ...extra});
      return prevented;
    },
    selected(expected) {
      for (const name of names) {
        const tab = tabs.find(t => t.dataset.projectTab === name), selected = name === expected;
        assert.equal(tab.getAttribute('aria-selected'), String(selected));
        assert.equal(tab.tabIndex, selected ? 0 : -1);
        assert.equal(nodes.get(tab.getAttribute('aria-controls')).hidden, !selected);
      }
    },
  };
}

const page = browser();page.selected('overview');
for (const name of names) {
  assert.equal(page.click(name), true);page.selected(name);
  assert.equal(new URL(page.location.href).searchParams.get('tab'), name);
  browser(page.location.href).selected(name);
}
page.history.back();page.selected('sessions');
page.history.forward();page.selected('knowledge');
page.key('ArrowRight');page.selected('overview');
page.key('ArrowLeft');page.selected('knowledge');
page.key('Home');page.selected('overview');
page.key('End');page.selected('knowledge');
page.key(' ');page.selected('knowledge');
for (const extra of [{altKey:true}, {ctrlKey:true}, {metaKey:true}]) {
  const original = page.location.href;
  assert.equal(page.key('ArrowLeft', extra), false);page.selected('knowledge');
  assert.equal(page.location.href, original);
}
for (const extra of [{ctrlKey:true}, {metaKey:true}, {shiftKey:true}, {button:1}]) {
  const original = page.location.href;
  assert.equal(page.click('agents', extra), false);
  assert.equal(page.location.href, original);page.selected('knowledge');
}
for (const [hash, name] of [['agents','agents'], ['token-usage','tokens'], ['session-token-table','sessions'], ['knowledge','knowledge']]) {
  const legacy = browser('https://memory.test/w/team/app#' + hash);legacy.selected(name);
  assert.equal(new URL(legacy.location.href).searchParams.get('tab'), name);
  assert.equal(new URL(legacy.location.href).hash, '');
}
browser('https://memory.test/w/team/app?tab=missing').selected('overview');
browser('https://memory.test/w/team/app?tab=agents#project-search-q').selected('knowledge');
browser('https://memory.test/w/team/app#%E0%A4%A').selected('overview');
const historyPage = browser();historyPage.click('tokens');
historyPage.document.activeElement = historyPage.nodes.get('project-panel-tokens');
historyPage.history.back();historyPage.selected('overview');
assert.equal(historyPage.document.activeElement, historyPage.tabs[0]);
historyPage.history.forward();historyPage.emit('pageshow');historyPage.selected('tokens');
