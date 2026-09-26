plugins {
    id("java-library-conventions")
    id("publishing-conventions")
}

tasks.processResources {
    val binary = project.rootProject.file(
        (project.findProperty("steelProvider") as String?)
            ?: "../steel-provider/target/debug/steel-provider"
    )

    doFirst {
        if (!binary.isFile) {
            throw GradleException(
                "steel-provider executable not found at ${binary.path}. " +
                    "Build it first, e.g. `mise run compile-rust --release --static`."
            )
        }
    }
    from(binary) {
        into("native")
    }
    from(project.rootProject.file("../steel-provider/LICENSE-AGPL")) {
        into("META-INF")
        rename { "LICENSE-AGPL.txt" }
    }
}
