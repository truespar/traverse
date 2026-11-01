package com.truespar.traverse;

/**
 * Exception thrown by Traverse native operations.
 */
public class TraverseException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final String gqlStatus;

    public TraverseException(String message) {
        this(message, (String) null);
    }

    public TraverseException(String message, String gqlStatus) {
        super(message);
        this.gqlStatus = gqlStatus;
    }

    public TraverseException(String message, Throwable cause) {
        super(message, cause);
        this.gqlStatus = null;
    }

    /**
     * The GQLSTATUS (ISO/IEC 39075): five characters naming the condition, such as
     * {@code 22012} (division by zero) or {@code 42N62} (variable not defined); {@code null} when
     * the error has none.
     */
    public String getGqlStatus() {
        return gqlStatus;
    }
}
