package com.truespar.traverse.http;

import tools.jackson.core.type.TypeReference;
import tools.jackson.databind.DeserializationFeature;
import tools.jackson.databind.ObjectMapper;

import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;
import java.io.*;
import java.net.URI;
import java.net.URLEncoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.security.cert.X509Certificate;
import java.time.Duration;
import java.util.*;

/**
 * HTTP client for the Traverse server management API.
 * Covers health, databases, files, aliases, schema, query, import, users, and server operations.
 */
public final class TraverseHttpClient implements AutoCloseable {
    private final HttpClient http;
    private final URI baseUri;
    private final String apiKey;
    private final Duration apiTimeout;
    private final Duration transferTimeout;
    private final ObjectMapper mapper;

    public TraverseHttpClient(String baseUrl) {
        this(baseUrl, null);
    }

    public TraverseHttpClient(String baseUrl, TraverseHttpOptions options) {
        if (options == null) options = new TraverseHttpOptions();
        this.apiKey = options.apiKey();
        this.apiTimeout = options.timeout() != null ? options.timeout() : Duration.ofSeconds(100);
        this.transferTimeout = options.fileTransferTimeout() != null ? options.fileTransferTimeout() : Duration.ofHours(1);

        var normalized = baseUrl.endsWith("/") ? baseUrl : baseUrl + "/";
        this.baseUri = URI.create(normalized);

        var builder = HttpClient.newBuilder();
        if (options.allowUntrustedCertificate()) {
            try {
                var ctx = SSLContext.getInstance("TLS");
                ctx.init(null, new TrustManager[]{new X509TrustManager() {
                    public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }
                    public void checkClientTrusted(X509Certificate[] c, String t) {}
                    public void checkServerTrusted(X509Certificate[] c, String t) {}
                }}, null);
                builder.sslContext(ctx);
            } catch (Exception e) {
                throw new TraverseHttpException("Failed to configure trust-all SSL", e);
            }
        }
        this.http = builder.build();
        this.mapper = createMapper();
    }

    @Override
    public void close() {
        http.close();
    }

    // -- Health ----------------------------------------------------------

    public HealthResponse health() {
        return get("api/health", HealthResponse.class);
    }

    public boolean ready() {
        try {
            var req = request("api/ready").timeout(apiTimeout).GET().build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            return resp.statusCode() >= 200 && resp.statusCode() < 300;
        } catch (Exception e) {
            return false;
        }
    }

    // -- Databases -------------------------------------------------------

    public List<DatabaseInfo> listDatabases() {
        return getList("api/databases", new TypeReference<>() {});
    }

    public void createDatabase(String name) {
        post("api/databases", Map.of("name", name));
    }

    public void dropDatabase(String name) {
        pollTask(delete("api/databases/" + encode(name) + "?async=true"));
    }

    public void renameDatabase(String oldName, String newName) {
        post("api/databases/" + encode(oldName) + "/rename", Map.of("name", newName));
    }

    public void unloadDatabase(String name) {
        pollTask(postRaw("api/databases/" + encode(name) + "/unload?async=true", null, transferTimeout));
    }

    public void loadDatabase(String name, String file) {
        pollTask(postRaw("api/databases/load?async=true", Map.of("name", name, "file", file), transferTimeout));
    }

    // -- Files -----------------------------------------------------------

    public List<TvdbFileInfo> listFiles() {
        return getList("api/files", new TypeReference<>() {});
    }

    public UploadResult uploadFile(String name, InputStream data) {
        return uploadFile(name, data, "database");
    }

    /**
     * Upload a .tvdb database ({@code kind = "database"}) or a
     * .tvmodel trained model ({@code kind = "model"} - requires a
     * server built with the ml feature).
     */
    public UploadResult uploadFile(String name, InputStream data, String kind) {
        try {
            var bytes = data.readAllBytes();
            var req = request("api/files/" + encode(name) + kindQuery(kind))
                    .timeout(transferTimeout)
                    .header("Content-Type", "application/octet-stream")
                    .PUT(HttpRequest.BodyPublishers.ofByteArray(bytes))
                    .build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
            return mapper.readValue(resp.body(), UploadResult.class);
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    public byte[] downloadFile(String name) {
        return downloadFile(name, "database");
    }

    /** Download a .tvdb database or ({@code kind = "model"}) a .tvmodel file. */
    public byte[] downloadFile(String name, String kind) {
        try {
            var req = request("api/files/" + encode(name) + kindQuery(kind))
                    .timeout(transferTimeout)
                    .GET().build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofByteArray());
            if (resp.statusCode() < 200 || resp.statusCode() >= 300)
                throw new TraverseHttpException("HTTP " + resp.statusCode(), resp.statusCode());
            return resp.body();
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    public void deleteFile(String name) {
        deleteFile(name, "database");
    }

    /** Delete a .tvdb database or ({@code kind = "model"}) a .tvmodel file. */
    public void deleteFile(String name, String kind) {
        delete("api/files/" + encode(name) + kindQuery(kind));
    }

    /**
     * Query-string suffix for /api/files kind routing. The default
     * "database" is omitted so requests stay byte-identical to
     * pre-kind clients.
     */
    private static String kindQuery(String kind) {
        return (kind == null || kind.equals("database")) ? "" : "?kind=" + encode(kind);
    }

    // -- Aliases ---------------------------------------------------------

    public List<AliasInfo> listAliases() {
        return getList("api/aliases", new TypeReference<>() {});
    }

    public void createAlias(String alias, String database) {
        post("api/aliases", Map.of("alias", alias, "database", database));
    }

    public void alterAlias(String alias, String database) {
        put("api/aliases/" + encode(alias), Map.of("database", database));
    }

    public void dropAlias(String alias) {
        delete("api/aliases/" + encode(alias));
    }

    // -- Schema ----------------------------------------------------------

    public SchemaResponse getSchema(String database) {
        var path = database != null
                ? "api/schema?database=" + encode(database)
                : "api/schema";
        return get(path, SchemaResponse.class);
    }

    // -- Query -----------------------------------------------------------

    public QueryResponse query(String cypher) {
        return query(cypher, null, null);
    }

    public QueryResponse query(String cypher, Map<String, Object> parameters, String database) {
        return query(cypher, parameters, database, null);
    }

    /**
     * Run a query. {@code dialect} selects the query language:
     * {@code null} (openCypher, the default) or {@code "gql"} for
     * ISO/IEC 39075 GQL - requires a server built with the gql
     * feature; check {@code health().features.get("gql")}.
     */
    public QueryResponse query(String cypher, Map<String, Object> parameters, String database, String dialect) {
        var body = new LinkedHashMap<String, Object>();
        body.put("query", cypher);
        if (parameters != null) body.put("parameters", parameters);
        if (database != null) body.put("database", database);
        if (dialect != null) body.put("dialect", dialect);
        return post("api/query", body, QueryResponse.class);
    }

    // -- Import ----------------------------------------------------------

    public ImportResult importCypher(String cypher, String database) {
        var body = new LinkedHashMap<String, String>();
        body.put("cypher", cypher);
        if (database != null) body.put("database", database);
        return post("api/import", body, ImportResult.class);
    }

    public CsvImportResult importCsv(String csv, String label, CsvImportResult.ColumnMapping[] columns,
                                      String database, String delimiter, Boolean skipHeader) {
        var body = new LinkedHashMap<String, Object>();
        body.put("csv", csv);
        body.put("label", label);
        body.put("columns", List.of(columns));
        if (database != null) body.put("database", database);
        if (delimiter != null) body.put("delimiter", delimiter);
        if (skipHeader != null) body.put("skip_header", skipHeader);
        return post("api/import/csv", body, CsvImportResult.class);
    }

    public CsvImportResult.EdgeResult importCsvEdges(String csv, String edgeType,
                                                      CsvImportResult.EdgeEndpoint source,
                                                      CsvImportResult.EdgeEndpoint target,
                                                      CsvImportResult.ColumnMapping[] columns,
                                                      String database, String delimiter, Boolean skipHeader) {
        var body = new LinkedHashMap<String, Object>();
        body.put("csv", csv);
        body.put("edge_type", edgeType);
        body.put("source", source);
        body.put("target", target);
        if (columns != null) body.put("columns", List.of(columns));
        if (database != null) body.put("database", database);
        if (delimiter != null) body.put("delimiter", delimiter);
        if (skipHeader != null) body.put("skip_header", skipHeader);
        return post("api/import/csv/edges", body, CsvImportResult.EdgeResult.class);
    }

    // -- Users -----------------------------------------------------------

    public List<UserInfo> listUsers() {
        return getList("api/users", new TypeReference<>() {});
    }

    public void createUser(String username, String password, String role) {
        post("api/users", Map.of("username", username, "password", password, "role", role));
    }

    public void updateUser(String username, String password, String role) {
        var body = new LinkedHashMap<String, String>();
        if (password != null) body.put("password", password);
        if (role != null) body.put("role", role);
        put("api/users/" + encode(username), body);
    }

    public void deleteUser(String username) {
        delete("api/users/" + encode(username));
    }

    // -- Server operations -----------------------------------------------

    public void save() {
        pollTask(postRaw("api/save?async=true", null, transferTimeout));
    }

    // -- Tasks -----------------------------------------------------------

    public TaskInfo getTask(String taskId) {
        return get("api/tasks/" + encode(taskId), TaskInfo.class);
    }

    // -- Internal HTTP helpers -------------------------------------------

    private URI uri(String path) {
        return baseUri.resolve(path);
    }

    private HttpRequest.Builder request(String path) {
        var b = HttpRequest.newBuilder().uri(uri(path));
        if (apiKey != null) b.header("Authorization", "Bearer " + apiKey);
        return b;
    }

    private static String encode(String s) {
        return URLEncoder.encode(s, StandardCharsets.UTF_8).replace("+", "%20");
    }

    private <T> T get(String path, Class<T> type) {
        try {
            var req = request(path).timeout(apiTimeout).GET().build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
            return mapper.readValue(resp.body(), type);
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private <T> List<T> getList(String path, TypeReference<List<T>> type) {
        try {
            var req = request(path).timeout(apiTimeout).GET().build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
            return mapper.readValue(resp.body(), type);
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private void post(String path, Object body) {
        postRaw(path, body, apiTimeout);
    }

    private <T> T post(String path, Object body, Class<T> type) {
        try {
            var json = mapper.writeValueAsString(body);
            var req = request(path)
                    .timeout(apiTimeout)
                    .header("Content-Type", "application/json")
                    .POST(HttpRequest.BodyPublishers.ofString(json))
                    .build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
            return mapper.readValue(resp.body(), type);
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private String postRaw(String path, Object body, Duration timeout) {
        try {
            var publisher = body != null
                    ? HttpRequest.BodyPublishers.ofString(mapper.writeValueAsString(body))
                    : HttpRequest.BodyPublishers.noBody();
            var req = request(path)
                    .timeout(timeout)
                    .header("Content-Type", "application/json")
                    .POST(publisher)
                    .build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
            return resp.body();
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private void put(String path, Object body) {
        try {
            var json = mapper.writeValueAsString(body);
            var req = request(path)
                    .timeout(apiTimeout)
                    .header("Content-Type", "application/json")
                    .PUT(HttpRequest.BodyPublishers.ofString(json))
                    .build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private String delete(String path) {
        try {
            var req = request(path).timeout(apiTimeout).DELETE().build();
            var resp = http.send(req, HttpResponse.BodyHandlers.ofString());
            ensureSuccess(resp);
            return resp.body();
        } catch (TraverseHttpException e) { throw e; }
        catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private void ensureSuccess(HttpResponse<String> resp) {
        if (resp.statusCode() >= 200 && resp.statusCode() < 300) return;
        String errorMessage = null;
        try {
            var node = mapper.readTree(resp.body());
            if (node.has("error")) errorMessage = node.get("error").asString();
        } catch (Exception ignored) {}
        throw new TraverseHttpException(
                errorMessage != null ? errorMessage : "HTTP " + resp.statusCode(),
                resp.statusCode());
    }

    private void pollTask(String responseBody) {
        if (responseBody == null || responseBody.isBlank()) return;
        try {
            var node = mapper.readTree(responseBody);
            if (!node.has("taskId")) return;
            var taskId = node.get("taskId").asString();
            int delayMs = 250;
            while (true) {
                Thread.sleep(delayMs);
                var task = getTask(taskId);
                if ("completed".equals(task.status())) return;
                if ("failed".equals(task.status()))
                    throw new TraverseHttpException(task.error() != null ? task.error() : "task failed");
                delayMs = Math.min(delayMs * 2, 2000);
            }
        } catch (TraverseHttpException e) { throw e; }
        catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new TraverseHttpException("interrupted", e);
        } catch (Exception e) { throw new TraverseHttpException(e.getMessage(), e); }
    }

    private static ObjectMapper createMapper() {
        return tools.jackson.databind.json.JsonMapper.builder()
                .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
                .build();
    }
}
