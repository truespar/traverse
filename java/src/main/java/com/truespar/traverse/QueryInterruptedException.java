package com.truespar.traverse;

/**
 * A statement stopped before it finished: it ran past {@link Database#getStatementTimeout()}, it
 * was cancelled ({@link Transaction#cancel()}), or its transaction was terminated
 * ({@code TERMINATE TRANSACTIONS}). A statement that had already changed data fails its
 * transaction.
 */
public class QueryInterruptedException extends TraverseException {
    private static final long serialVersionUID = 1L;

    public QueryInterruptedException(String message) {
        super(message);
    }

    public QueryInterruptedException(String message, String gqlStatus) {
        super(message, gqlStatus);
    }
}
