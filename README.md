# SteelWorldGen

![GitHub License](https://img.shields.io/badge/license-Apache--2.0-green)\*
![GitHub last commit](https://img.shields.io/github/last-commit/BlueDragonMC/SteelWorldGen)
![Minestom version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FBlueDragonMC%2FSteelWorldGen%2Fmain%2Fjava-client%2Fgradle%2Flibs.versions.toml&query=%24.versions.minestom&label=Minestom%20Version)

Uses [SteelMC](https://github.com/Steel-Foundation/SteelMC/) as a library to implement vanilla Minecraft world generation in a Minestom world generator.

<small>

_\* The Java libraries in this repo are Apache-2.0, but SteelMC itself is licensed under the AGPLv3 license. See [LICENSE.md](./LICENSE.md) for more details._

</small>

## How it works

`steel-provider/src/lib.rs` contains some functions that interact with SteelMC to bring chunks through the full generation process outside of a normal server environment. Those functions are compiled into a standalone executable (`steel-provider/src/main.rs`), which acts as a "dumb" server that exclusively handles chunk generation.

The Java side is split into three modules in `java-client`: 
1. the `native` module packages the AGPL-3.0 `steel-provider` executable,
2. the `bridge` module is a standalone Apache-2.0 client library that connects to the steel-provider server, and
3. the `minestom` module adapts it into a Minestom world generator. `bridge` depends on `native` at runtime so the embedded server works out of the box.

Each `Generator#generate()` call sends a small packet with the seed and chunk coordinates and then reads a response containing the generated chunk's sections in Minecraft's own network format.

The server can be used standalone. Currently, only a Java client exists, but other clients could easily be made as long as they understand how to decode the data structures in Minecraft's chunk data packet. For more details on the protocol, see [steel-provider/PROTOCOL.md](steel-provider/PROTOCOL.md).

## Installation

![Latest version](https://img.shields.io/badge/dynamic/xml?url=https%3A%2F%2Freposilite.bluedragonmc.com%2Freleases%2Fcom%2Fbluedragonmc%2Fsteelworldgen-minestom%2Fmaven-metadata.xml&query=%2Fmetadata%2Fversioning%2Flatest&label=Latest%20Version)

```kotlin
repositories {
   maven(url = "https://reposilite.bluedragonmc.com/releases")
}

dependencies {
   implementation("com.bluedragonmc:steelworldgen-minestom:$VERSION")
}
```

If you only need to talk to a steel-provider server without Minestom, depend on `com.bluedragonmc:steelworldgen-bridge` instead.

## Usage

```java
long seed = 42L;

Instance overworld = MinecraftServer.getInstanceManager().createInstanceContainer();
overworld.setGenerator(SteelWorldGenProvider.getGenerator(42L));
overworld.setChunkSupplier(LightingChunk::new);

Instance nether = MinecraftServer.getInstanceManager().createInstanceContainer(DimensionType.THE_NETHER);
nether.setGenerator(SteelWorldGenProvider.getGenerator(seed, Dimension.NETHER));
nether.setChunkSupplier(LightingChunk::new);

Instance theEnd = MinecraftServer.getInstanceManager().createInstanceContainer(DimensionType.THE_END);
theEnd.setGenerator(SteelWorldGenProvider.getGenerator(seed, Dimension.THE_END));
theEnd.setChunkSupplier(LightingChunk::new);
```

For a full example, see the `java-client/demo` directory. You can run the demo locally with `mise run demo`.
You'll probably want to use the `--release` flag (`mise run demo --release`).
Chunk generation gets MUCH faster at the expense of a longer compilation time.

## Building from Source

1. Install [`mise`](https://mise.jdx.dev/)

   We use `mise` to manage tools (like Java and Gradle) and to define tasks like you would in a Makefile.
   It's configured in `mise.toml`.

2. Run `mise run build`

   For a release (optimized) build, use `mise run build --release`.

   By default the Rust binary is built natively with `cargo build`. To instead cross-compile a fully static binary using [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), pass `--static` (`mise run build --release --static`).

   The Java library will be built to `java-client/minestom/build/libs/minestom-dev.jar`. If you want to publish it to a Maven repository, modify the hostname in [java-client/minestom/build.gradle.kts](java-client/minestom/build.gradle.kts) and run `mise run publish` (or `mise run publishToMavenLocal` to run `gradle publishToMavenLocal`).

## Performance

This project generates chunks much faster than vanilla Minecraft, but slower than a regular SteelMC server. `steel-provider` itself (the Rust side of this project) reaches almost the same throughput as standalone SteelMC when it is given enough concurrent requests. However, when using it from Minestom, the networking and conversion come with a performance penalty.

On my ThinkPad P1 Gen 6 laptop with an Intel Core i7-13800H (20 logical CPU cores), I can generate:

- Steel: 908 chunks/second
- Fabric: 86 chunks/second
- steel-provider (my Steel wrapper): 854 chunks/second
- steel-provider + Minestom world generator wrapper: 494 chunks/second

Reproduce the benchmarks yourself using `mise run bench`. It takes me about 10 minutes to run all 3 trials.

### AI Disclosure

The Rust portion of this project was written with a lot of AI assistance.
