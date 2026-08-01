package com.bluedragonmc.server.worldgen;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;

public class NativeLoader {

    public static void loadLibraryFromJar(String pathInJar, String fileName) {
        try (InputStream is = NativeLoader.class.getResourceAsStream(pathInJar)) {
            if (is == null) {
                throw new RuntimeException("Library not found in JAR: " + pathInJar);
            }

            // Create a temporary file to hold the extracted library
            String prefix = fileName.substring(0, fileName.indexOf('.'));
            String suffix = fileName.substring(fileName.indexOf('.'));
            Path tempFile = Files.createTempFile(prefix, suffix);
            
            // Ensure the temporary file is deleted when the JVM exits
            tempFile.toFile().deleteOnExit();

            // Copy the library from the JAR to the temporary file
            Files.copy(is, tempFile, StandardCopyOption.REPLACE_EXISTING);

            // Load the library from the absolute path of the temporary file
            System.load(tempFile.toAbsolutePath().toString());

        } catch (IOException e) {
            throw new RuntimeException("Failed to load native library", e);
        }
    }
}