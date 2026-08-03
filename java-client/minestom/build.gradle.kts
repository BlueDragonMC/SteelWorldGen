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
