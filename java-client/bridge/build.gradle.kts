plugins {
    id("java-library-conventions")
    id("publishing-conventions")
}

dependencies {
    runtimeOnly(project(":native"))
}

tasks.processResources {
    from(rootProject.file("../LICENSE-Apache-2.0")) {
        into("META-INF")
        rename { "LICENSE-Apache-2.0.txt" }
    }
}
