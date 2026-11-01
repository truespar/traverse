package com.truespar.traverse.http;

/**
 * Exception thrown by {@link TraverseHttpClient} for HTTP and API errors.
 */
public class TraverseHttpException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final int statusCode;

    public TraverseHttpException(String message) {
        super(message);
        this.statusCode = 0;
    }

    public TraverseHttpException(String message, int statusCode) {
        super(message);
        this.statusCode = statusCode;
    }

    public TraverseHttpException(String message, Throwable cause) {
        super(message, cause);
        this.statusCode = 0;
    }

    /** HTTP status code, or 0 if not an HTTP error. */
    public int statusCode() {
        return statusCode;
    }
}
