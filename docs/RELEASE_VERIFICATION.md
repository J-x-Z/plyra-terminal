# Release verification

Plyra Terminal releases use FileVeil Ed25519 provenance under the project identifier `org.plyra.terminal`. Pin this publisher key when checking the first release.

```text
9f0095abf8810dd0cf857ed0e4248846de2ad076ffaa661b36c9c77aac8ee9c3
```

Download the explicit source archive and its FileVeil evidence bundle from the release page. Verify the envelope against `manifest.json` using the independent verifier in the [FileVeil repository](https://github.com/J-x-Z/fileveil).

```sh
python3 /path/to/fileveil/tools/verify-evidence.py evidence.json --asset manifest.json
```

Compare the envelope signer with the pinned key, then compare the downloaded archive's SHA-256 with its entry in the authenticated manifest. The explicit release archive is the payload named in that manifest.

For individual source files, `fileveil verify FILE --json` checks the embedded record. `provenance/source-inventory.json` records the expected path, original hash, distribution hash, and any compatible filename suffix required by the verifier for shader or script languages. Its hashes also cover files that carry no inline record. The archive binds this inventory and the embedded records together.
