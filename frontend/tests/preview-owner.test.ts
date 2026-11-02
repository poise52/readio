import { test } from 'node:test'
import { strict as assert } from 'node:assert'
import { PreviewOwner } from '../app/utils/preview-owner.ts'

test('preview cancellation stops its service and serializes the next preview', async () => {
  let calls = 0, release!: () => void
  const blocked = new Promise<void>(resolve => { release = resolve })
  const owner = new PreviewOwner(async () => { calls++; await blocked }, () => {})
  owner.start(1)
  owner.cancel()
  owner.start(2)
  owner.finish(1)
  assert.equal(owner.active, true)
  let ready = false
  const wait = owner.wait().then(() => { ready = true })
  await Promise.resolve()
  assert.equal(calls, 1)
  assert.equal(ready, false)
  release()
  await wait
  assert.equal(ready, true)
  owner.finish(2)
  assert.equal(owner.active, false)
})
