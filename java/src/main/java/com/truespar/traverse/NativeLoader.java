package com.truespar.traverse;

import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import java.util.Locale;

/**
 * Loads the JNI library, from the first of:
 * <ol>
 *   <li>the file named by the {@code traverse.library} system property or the
 *       {@code TRAVERSE_JAVA_LIB} environment variable (a library built from source, a platform the
 *       jar doesn't bundle);</li>
 *   <li>the copy bundled in the jar, extracted once per version into a per-user cache;</li>
 *   <li>{@code traverse_java} on {@code java.library.path}.</li>
 * </ol>
 */
final class NativeLoader {
    static final String PATH_PROPERTY = "traverse.library";
    static final String PATH_VARIABLE = "TRAVERSE_JAVA_LIB";

    private static boolean loaded;

    private NativeLoader() {}

    @SuppressWarnings("restricted") // JNI: callers run with --enable-native-access
    static synchronized void load() {
        if (loaded) return;
        String path = System.getProperty(PATH_PROPERTY);
        if (path == null || path.isEmpty()) path = System.getenv(PATH_VARIABLE);
        if (path != null && !path.isEmpty()) {
            System.load(Path.of(path).toAbsolutePath().toString());
        } else {
            Path bundled = extractBundled();
            if (bundled != null) {
                System.load(bundled.toString());
            } else {
                System.loadLibrary("traverse_java");
            }
        }
        loaded = true;
    }

    /**
     * The bundled library for this platform as a file, or {@code null} when the jar has none. The
     * file is named after its content, so each version is extracted once: a loaded library can't
     * be deleted on Windows, and a copy per run would pile up.
     */
    private static Path extractBundled() {
        String os = detectOs();
        String libName = mapLibraryName(os);
        String resource = "/natives/" + os + "-" + detectArch() + "/" + libName;
        byte[] bytes;
        try (InputStream in = NativeLoader.class.getResourceAsStream(resource)) {
            if (in == null) return null;
            bytes = in.readAllBytes();
        } catch (IOException e) {
            throw new UncheckedIOException("reading " + resource, e);
        }
        String version = HexFormat.of().formatHex(sha256(bytes), 0, 8);
        try {
            Path dir = cacheDir().resolve(version);
            Path lib = dir.resolve(libName);
            if (!isCopy(lib, bytes)) {
                Files.createDirectories(dir);
                Path tmp = Files.createTempFile(dir, libName, ".tmp");
                try {
                    Files.write(tmp, bytes);
                    Files.move(tmp, lib, StandardCopyOption.ATOMIC_MOVE);
                } catch (IOException e) {
                    // another process got there first (and may have it loaded)
                    if (!isCopy(lib, bytes)) throw e;
                } finally {
                    Files.deleteIfExists(tmp);
                }
            }
            return lib;
        } catch (IOException e) {
            return extractToTempDir(libName, bytes, e);
        }
    }

    /** No writable cache (a read-only home, say): one temp copy per run, as a last resort. */
    private static Path extractToTempDir(String libName, byte[] bytes, IOException cacheError) {
        try {
            Path dir = Files.createTempDirectory("traverse-jni");
            Path lib = dir.resolve(libName);
            Files.write(lib, bytes);
            lib.toFile().deleteOnExit();
            dir.toFile().deleteOnExit();
            return lib;
        } catch (IOException e) {
            e.addSuppressed(cacheError);
            throw new UncheckedIOException("extracting the Traverse native library", e);
        }
    }

    private static boolean isCopy(Path lib, byte[] bytes) throws IOException {
        // the content is in the directory name and files get there whole (a rename), so a
        // matching size means a finished copy
        return Files.isRegularFile(lib) && Files.size(lib) == bytes.length;
    }

    /** A cache only this user writes to: the jar's library is loaded from it. */
    private static Path cacheDir() {
        String home = System.getProperty("user.home");
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        Path base;
        if (os.contains("win")) {
            String local = System.getenv("LOCALAPPDATA");
            base = local != null && !local.isEmpty() ? Path.of(local) : Path.of(home, "AppData", "Local");
        } else if (os.contains("mac") || os.contains("darwin")) {
            base = Path.of(home, "Library", "Caches");
        } else {
            String xdg = System.getenv("XDG_CACHE_HOME");
            base = xdg != null && xdg.startsWith("/") ? Path.of(xdg) : Path.of(home, ".cache");
        }
        return base.resolve("traverse").resolve("jni");
    }

    private static byte[] sha256(byte[] bytes) {
        try {
            return MessageDigest.getInstance("SHA-256").digest(bytes);
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException("every JVM has SHA-256", e);
        }
    }

    private static String detectOs() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        if (os.contains("win")) return "windows";
        if (os.contains("mac") || os.contains("darwin")) return "macos";
        if (os.contains("linux")) return "linux";
        throw new UnsupportedOperationException(
                "Traverse has no native library for " + os + "; set " + PATH_VARIABLE);
    }

    private static String detectArch() {
        String arch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
        if (arch.equals("amd64") || arch.equals("x86_64")) return "x86_64";
        if (arch.equals("aarch64") || arch.equals("arm64")) return "aarch64";
        throw new UnsupportedOperationException(
                "Traverse has no native library for " + arch + "; set " + PATH_VARIABLE);
    }

    private static String mapLibraryName(String os) {
        return switch (os) {
            case "windows" -> "traverse_java.dll";
            case "macos" -> "libtraverse_java.dylib";
            case "linux" -> "libtraverse_java.so";
            default -> throw new UnsupportedOperationException("Unsupported OS: " + os);
        };
    }
}
