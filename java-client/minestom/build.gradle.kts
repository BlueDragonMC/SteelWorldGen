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

// The minestom jar is distributed separately from bridge, so it needs its own
// copy of the Apache License text.
tasks.processResources {
    from(rootProject.file("../LICENSE-Apache-2.0")) {
        into("META-INF")
        rename { "LICENSE-Apache-2.0.txt" }
    }
}
