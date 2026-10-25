# Native profile guided builds

`scripts/pgo.py` builds an optional native binary using execution counters collected from a training workload. The default Cargo and container builds remain unchanged. Profiles are specific to the source, compiler, features and release flags; the helper rejects a different generation when building the optimized binary.

The helper instruments the final `rgnix` crate, including its instantiated generic HTTP code. Dependencies retain their normal release builds. This avoids rebuilding the entire dependency graph, but is not whole-program instrumentation. Wasmtime's generated plugin machine code is outside this build profile.

## Collect a profile

Run on the Linux architecture that will execute the binary. Use a fresh native directory, with Rust, Cargo and Git available. `llvm-profdata` must match rustc's LLVM major version; the helper prefers the installed `llvm-tools` component and then checks the system tool. `--llvm-profdata` selects an explicit executable.

```sh
python3 scripts/pgo.py generate --work-dir /var/tmp/rgnix-pgo
```

The instrumented binary is `/var/tmp/rgnix-pgo/rgnix-pgo-generate`. Run it against a representative configuration and workload, then stop it gracefully. Execution counters are written to `/var/tmp/rgnix-pgo/profiles`. Do not override `LLVM_PROFILE_FILE` to another directory.

For example, the existing comparison tool can collect plain proxy traffic without requiring OpenResty:

```sh
python3 scripts/benchmark_compare.py \
  --rgnix /var/tmp/rgnix-pgo/rgnix-pgo-generate \
  --rgnix-transport hyper --engines rgnix --plain-proxy \
  --nginx /usr/sbin/nginx --openresty /usr/sbin/nginx \
  --cases proxy-1k proxy-16k --concurrency 16 128 \
  --workers 1 --origin-workers 2 --client-threads 2 \
  --server-cpus 10 --client-cpus 2,3 --origin-cpus 6,7 \
  --rounds 1 --warmup 2 --seconds 8 \
  --work-dir /var/tmp/rgnix-pgo-training \
  --output /var/tmp/rgnix-pgo-training.json
```

Adjust CPU sets to the available machine. Include TLS, HTTP/2, RGL and policies in training when they are part of the intended workload. Instrumented throughput is training data, not a performance result.

## Build and compare

```sh
python3 scripts/pgo.py use --work-dir /var/tmp/rgnix-pgo
```

This merges counters and produces `/var/tmp/rgnix-pgo/rgnix-pgo-use`, with compiler diagnostics in `pgo-use.log`. `pgo.json` records source, compiler, flags, profile and binary digests plus build commands. `--target-dir` reuses an existing Cargo cache; `--offline` uses only cached dependencies. Both stages must use identical `--features` (default: `hyper-experimental,jemalloc`). No host-specific CPU instruction flags are added.

Verify the optimized binary's actual behavior and compare it with an ordinary release binary using the same configuration and CPU sets. Repeat identical binaries at each concurrency before attributing throughput changes to PGO. Keep all measurement windows and errors. A profile that helps one workload does not establish a gain for other protocols or deployments.

The Hyper engine still requires `--experimental-hyper`; building with PGO does not select it automatically. Recollect profiles after changing compilation inputs. PGO binaries and counters are build artifacts, and are not committed as a portable release.
