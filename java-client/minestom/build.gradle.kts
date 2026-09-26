plugins {
    id("java-library-conventions")
    id("publishing-conventions")
}

repositories {
    maven(url = "https://reposilite.bluedragonmc.com/releases")
}

dependencies {
    api(project(":bridge"))
    compileOnly(libs.minestom)
    testImplementation(libs.minestom)
}

// Resolves the `net.minestom:data` jars that steel-provider's build script reads
// block and biome registry data from. The "from" (SteelMC) version must be given explicitly via
// `-PminecraftDataFromVersion`, since it is independent of the client.
val minecraftDataFrom = configurations.create("minecraftDataFrom") {
    isCanBeConsumed = false
    isCanBeResolved = true
}

// Inherits `minestom` and, transitively, the `data` jar for the client version.
val minecraftDataTo = configurations.create("minecraftDataTo") {
    isCanBeConsumed = false
    isCanBeResolved = true
}

dependencies {
    minecraftDataTo(libs.minestom)

    val fromVersion = findProperty("minecraftDataFromVersion") as String?
    if (!fromVersion.isNullOrBlank()) {
        minecraftDataFrom("net.minestom:data:$fromVersion")
    }
}

val resolveMinecraftData = tasks.register("resolveMinecraftData") {
    description = "Writes net.minestom:data jar paths for the from/to versions to build/minecraft-data.env."
    notCompatibleWithConfigurationCache("resolves detached configurations at execution time")

    val fromConf = minecraftDataFrom
    val toConf = minecraftDataTo
    val outputFile = layout.buildDirectory.file("minecraft-data.env")

    doLast {
        fun dataJar(name: String, files: Set<File>): File =
            files.firstOrNull { it.name.startsWith("data-") && it.extension == "jar" }
                ?: error("could not find the net.minestom:data jar on the $name classpath: $files")

        val fromJar = dataJar("from", fromConf.resolve())
        val toJar = dataJar("to", toConf.resolve())

        val target = outputFile.get().asFile
        target.parentFile.mkdirs()
        target.writeText(
            "MINESTOM_DATA_FROM_JAR=${fromJar.absolutePath}\n" +
                "MINESTOM_DATA_TO_JAR=${toJar.absolutePath}\n"
        )
    }
}

// The minestom jar is distributed separately from bridge, so it needs its own
// copy of the Apache License text.
tasks.processResources {
    from(rootProject.file("../LICENSE-Apache-2.0")) {
        into("META-INF")
        rename { "LICENSE-Apache-2.0.txt" }
    }
}
