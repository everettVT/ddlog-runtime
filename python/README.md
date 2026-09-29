# Runtime attachment client

`ddlog-runtime-client` is the shared, dependency-free Python client for the native
world owner. Observer and product workers use the same transport and admission
contract. It contains no HTTP server, process supervisor, provider implementation,
product schemas, or automatic mutation retry.

```python
from ddlog_runtime_client import AttachedRuntimeClient, ManagedWorldHost

client = AttachedRuntimeClient('/absolute/environment/owner/endpoint.json')
try:
    reply = client.request('status', {'id': world_id})
    if not reply['ok']:
        raise RuntimeError(reply['error'])
    status = reply['result']
    host = ManagedWorldHost(client, world_id, status['generation'])
    evidence = host.snapshot(['input_relation', 'output_relation'])
finally:
    client.close()  # Detaches; never stops the owner or its worlds.
```

The endpoint descriptor identifies one owner incarnation. Each request uses a
bounded connection and checks response identity. A timeout means the outcome may
be unknown; it does not close the owner's input or kill its process. The optional
timing callback receives content-free request timing.

`ManagedWorldHost.snapshot` reads complete public relations at one world generation
and committed revision. It pages through the owner's `read_batch`, rejects stale
or inconsistent evidence and enforces row/byte limits. It does not return a partial
table as a complete snapshot. A supervised worker passes its `worker_id` to the
constructor, so runtime cancellation fences subsequent reads and admissions.

`compare_apply` carries the snapshot's generation/revision and an admission key to
`admit_inputs`. Its result keeps native application, durable publication, replay,
and external-effect authority separate. Only a fresh durable effect reservation
with `effect_authorized: true` authorizes an external call. Neither a repeated
request nor an admission-status lookup grants that authority. See
[managed workers](../docs/managed-workers.md) for exact request shapes and limits.

Build this wheel before installing Observer or a product worker. Release all three
against their recorded source commits; a source-tree `PYTHONPATH` is a development
convenience, not the installed deployment contract.
