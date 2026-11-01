/**
 * Error thrown by TraverseHttpClient when the server returns a non-2xx response.
 */
export class TraverseHttpError extends Error {
  /**
   * @param {string} message - Error message (from server JSON "error" field, or fallback).
   * @param {number|null} [statusCode=null] - HTTP status code, if available.
   */
  constructor(message, statusCode = null) {
    super(message);
    this.name = 'TraverseHttpError';
    this.statusCode = statusCode;
  }
}
