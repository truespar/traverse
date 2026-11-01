package com.truespar.traverse;

import java.lang.ref.Cleaner;

/**
 * Frees native handles once their Java owner is unreachable. Explicit close/commit/abort only end
 * what a handle holds; the memory goes here, when no call can still be using it.
 */
final class Handles {
    static final Cleaner CLEANER = Cleaner.create();

    private Handles() {
    }
}
