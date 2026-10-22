# cortex9-epoch-gc

[![CI](https://github.com/federiconbaez/cortex9-epoch-gc/actions/workflows/ci.yml/badge.svg)](https://github.com/federiconbaez/cortex9-epoch-gc/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-Apache--2.0-blue)

**Reclamación de memoria por épocas** (EBR) y un **hash map lock-free** (listas de Harris–Michael) en Rust, sin dependencias. Verificado con **loom** (model checking de interleavings, con controles negativos), **Miri** (`-Zmiri-strict-provenance`) y **ThreadSanitizer** en CI.

**Epoch-based memory reclamation** (EBR) and a **lock-free hash map** (Harris–Michael lists) in Rust, with no dependencies. Checked with **loom** (interleaving model checking, with negative controls), **Miri** (`-Zmiri-strict-provenance`) and **ThreadSanitizer** in CI.

Parte de la serie Cortex-9 / Part of the Cortex-9 series: [github.com/federiconbaez](https://github.com/federiconbaez#cortex-9).

> **ES** primero · **EN** below

---

## ES

### Diseño

- **`epoch`**: un `Collector` con una época global que avanza de a 2 (el bit bajo de la palabra de cada participante significa "pinneado"). `Handle::pin` publica la época con un `swap` SeqCst seguido de un `fence(SeqCst)`; desanclar es un `store` Release. La época sólo pasa de `e` a `e + 2` si todos los participantes pinneados ya vieron `e`. La basura se etiqueta con la época al retirarla y se libera cuando la época global la supera en 4 (dos avances). El escaneo de participantes usa `try_lock`: si la lista está ocupada, no se avanza y nadie se bloquea.
- **Basura huérfana**: si un `Handle` se suelta con basura pendiente, pasa a una lista global que recolecta cualquier otro handle; al soltar el `Collector` se libera todo.
- **`map`**: `HashMap<V>` con claves `u64`, un número fijo de buckets (potencia de 2) y una lista ordenada de Harris–Michael por bucket. Borrar = marcar el bit bajo del puntero `next` y después desenganchar; cualquier recorrido que encuentre un nodo marcado ayuda a desengancharlo y lo retira con `Guard::defer`. El marcado preserva la *provenance* de los punteros (`AtomicPtr` + offsets de bytes, sin casts de entero a puntero), por eso pasa Miri en modo estricto.
- **`Bugs`**: dos fallas deliberadas (`ignore_pinned`: avanzar sin mirar a los lectores; `free_early`: liberar un avance antes) que usan los controles negativos para probar que los tests detectan lo que dicen detectar.

### Un bug real que encontró loom

La primera versión de `try_advance` no tenía un `fence(SeqCst)` al principio. En x86 no se nota (el CAS que desengancha el nodo ya es una barrera completa), pero en el modelo de memoria de C++/Rust el desenganche (CAS AcqRel) y la lectura de la palabra del lector se pueden reordenar: un lector puede pinnearse sin que el recolector lo vea y aun así encontrar el nodo ya retirado. El test de loom `reader_never_sees_freed_value` encontró ese interleaving; el commit que agrega el fence lo documenta.

### Tests

| Suite | Qué verifica | Cantidad |
|---|---|---|
| unit (`src/`) | la época espera a los pinneados, la basura se libera sólo tras dos avances, huérfanos, drop del collector, pin reentrante, operaciones del map | 7 |
| `tests/stress.rs` | 8 hilos insertando/borrando claves superpuestas (balance por clave), cada valor se destruye exactamente una vez y se recupera memoria en concurrencia, referencias válidas mientras se está pinneado | 3 |
| `tests/loom.rs` | lector vs. borrador en todos los interleavings; carrera insert/remove; **2 controles negativos** (`#[should_panic]` con `ignore_pinned` y `free_early`) | 4 |

Las mismas pruebas de unit y stress corren bajo Miri (tamaños reducidos con `cfg!(miri)`) y bajo TSan.

```sh
cargo test --release                                      # unit + stress
RUSTFLAGS="--cfg loom" cargo test --release --test loom   # loom
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test
RUSTFLAGS=-Zsanitizer=thread cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu --tests
cargo run --release --example bench
```

### Números medidos

Máquina: VM Linux 6.12, Intel Xeon, 8 vCPU. Rust 1.99.1 estable, nightly de 2026-10.

- loom: las 4 pruebas en ~34 s (release); los dos controles negativos fallan como se espera.
- Miri (`-Zmiri-strict-provenance`): unit + stress en ~1 min 46 s, sin UB.
- TSan: unit + stress sin reportes.
- `examples/bench.rs`: 2.000.000 operaciones (90 % get, 5 % insert, 5 % remove) sobre 65.536 claves, 16.384 buckets, en Mops/s (3 corridas; rango):

| hilos | este map | `Mutex<HashMap>` | `RwLock<HashMap>` |
|---|---|---|---|
| 1 | 26–29 | 37–39 | 38–42 |
| 2 | 31–33 | 6–8 | 8–9 |
| 4 | 65–78 | 4,7–4,9 | 5,6–6,0 |
| 8 | 69–92 | 3,6–4,1 | 4,8–5,4 |

Con un solo hilo el `HashMap` de std con lock es más rápido (sin contención el lock es casi gratis y las listas por bucket cuestan más que el hashing abierto); desde 2 hilos el map lock-free escala y los locks no. El número de 8 hilos es ruidoso en esta VM.

### Límites (v0.1)

- Cantidad de buckets fija: no hay resize. Con muchas más claves que buckets las listas crecen y el rendimiento cae linealmente.
- Claves `u64` solamente; `insert` no reemplaza (insert-if-absent).
- El avance de época usa `try_lock` sobre la lista de participantes: registrar/desregistrar handles toma un lock (pin/unpin no).
- Un hilo pinneado mucho tiempo impide liberar memoria (propiedad de EBR, no un bug).
- loom explora con sus cotas por defecto; no es una prueba formal.

---

## EN

### Design

- **`epoch`**: a `Collector` with a global epoch that advances in steps of 2 (the low bit of each participant's word means "pinned"). `Handle::pin` publishes the epoch with a SeqCst `swap` followed by `fence(SeqCst)`; unpinning is a Release `store`. The epoch only moves from `e` to `e + 2` if every pinned participant has already observed `e`. Garbage is tagged with the epoch at retirement and freed once the global epoch is 4 ahead (two advances). The participant scan uses `try_lock`: if the list is busy, the epoch simply does not advance and nobody blocks.
- **Orphaned garbage**: a `Handle` dropped with pending garbage hands it to a global list that any other handle collects; dropping the `Collector` frees everything.
- **`map`**: `HashMap<V>` with `u64` keys, a fixed (power-of-two) number of buckets and one sorted Harris–Michael list per bucket. Removing = marking the low bit of the `next` pointer, then unlinking; any traversal that meets a marked node helps unlink it and retires it with `Guard::defer`. Tagging preserves pointer provenance (`AtomicPtr` + byte offsets, no integer-to-pointer casts), which is why it passes Miri in strict mode.
- **`Bugs`**: two deliberate faults (`ignore_pinned`: advance without looking at readers; `free_early`: free one advance too early) used by the negative controls to show the tests catch what they claim to catch.

### A real bug loom found

The first version of `try_advance` had no `fence(SeqCst)` at its start. On x86 it does not show (the CAS that unlinks the node is already a full barrier), but in the C++/Rust memory model the unlink (AcqRel CAS) and the load of the reader's word can be reordered: a reader can pin without the collector seeing it and still find the already-retired node. The loom test `reader_never_sees_freed_value` found that interleaving; the commit adding the fence documents it.

### Tests

| Suite | What it checks | Count |
|---|---|---|
| unit (`src/`) | the epoch waits for pinned threads, garbage freed only after two advances, orphans, collector drop, re-entrant pin, map operations | 7 |
| `tests/stress.rs` | 8 threads inserting/removing overlapping keys (per-key balance), every value dropped exactly once with memory reclaimed concurrently, references valid while pinned | 3 |
| `tests/loom.rs` | reader vs. remover over all interleavings; insert/remove race; **2 negative controls** (`#[should_panic]` with `ignore_pinned` and `free_early`) | 4 |

The same unit and stress tests run under Miri (smaller sizes via `cfg!(miri)`) and under TSan. Commands are in the ES section.

### Measured numbers

Machine: Linux 6.12 VM, Intel Xeon, 8 vCPU. Rust 1.99.1 stable, nightly from 2026-10.

- loom: all 4 tests in ~34 s (release); both negative controls fail as expected.
- Miri (`-Zmiri-strict-provenance`): unit + stress in ~1 min 46 s, no UB.
- TSan: unit + stress, no reports.
- `examples/bench.rs`: 2,000,000 operations (90% get, 5% insert, 5% remove) over 65,536 keys, 16,384 buckets, in Mops/s (3 runs; range): see the table above. With one thread std's locked `HashMap` is faster (an uncontended lock is nearly free and per-bucket lists cost more than open hashing); from 2 threads on the lock-free map scales and the locks do not. The 8-thread figure is noisy on this VM.

### Limits (v0.1)

- Fixed bucket count: no resize. With many more keys than buckets the lists grow and performance degrades linearly.
- `u64` keys only; `insert` does not replace (insert-if-absent).
- Epoch advance uses `try_lock` on the participant list: registering/dropping handles takes a lock (pin/unpin do not).
- A thread pinned for a long time prevents freeing (an EBR property, not a bug).
- loom explores within its default bounds; this is not a formal proof.

## License

Apache-2.0. See [LICENSE](LICENSE).
