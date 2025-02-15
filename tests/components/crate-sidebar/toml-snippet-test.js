import { render } from '@ember/test-helpers';
import { module, test } from 'qunit';

import { hbs } from 'ember-cli-htmlbars';

import { setupRenderingTest } from 'crates-io/tests/helpers';

import setupMirage from '../../helpers/setup-mirage';

module('Component | CrateSidebar | toml snippet', function (hooks) {
  setupRenderingTest(hooks);
  setupMirage(hooks);

  test('show version number without build metadata', async function (assert) {
    let crate = this.server.create('crate', { name: 'foo' });
    this.server.create('version', { crate, num: '1.0.0+abcdef' });

    let store = this.owner.lookup('service:store');
    this.crate = await store.findRecord('crate', crate.name);
    this.version = (await this.crate.versions).firstObject;

    await render(hbs`<CrateSidebar @crate={{this.crate}} @version={{this.version}} />`);
    assert.dom('[title="Copy Cargo.toml snippet to clipboard"]').exists().hasText('foo = "1.0.0"');
  });

  test('show pre-release version number without build', async function (assert) {
    let crate = this.server.create('crate', { name: 'foo' });
    this.server.create('version', { crate, num: '1.0.0-alpha+abcdef' });

    let store = this.owner.lookup('service:store');
    this.crate = await store.findRecord('crate', crate.name);
    this.version = (await this.crate.versions).firstObject;

    await render(hbs`<CrateSidebar @crate={{this.crate}} @version={{this.version}} />`);
    assert.dom('[title="Copy Cargo.toml snippet to clipboard"]').exists().hasText('foo = "1.0.0-alpha"');
  });
});
