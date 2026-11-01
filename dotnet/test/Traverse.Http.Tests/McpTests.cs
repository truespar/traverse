using System.Net;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class McpTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;
    private static readonly JsonSerializerOptions JsonOpts = new() { PropertyNameCaseInsensitive = true };

    public McpTests(HttpFixture fixture) => _fixture = fixture;

    // -- Helpers ------------------------------------------------------

    private HttpClient CreateHttpClient(bool withAuth = true)
    {
        var client = new HttpClient
        {
            BaseAddress = new Uri(HttpFixture.BaseUrl.TrimEnd('/') + "/"),
            Timeout = TimeSpan.FromSeconds(10),
        };
        if (withAuth && _fixture.ApiKey is not null)
            client.DefaultRequestHeaders.Authorization =
                new AuthenticationHeaderValue("Bearer", _fixture.ApiKey);
        return client;
    }

    private static StringContent JsonRpc(string method, object? @params = null, object? id = null)
    {
        var body = JsonSerializer.Serialize(new
        {
            jsonrpc = "2.0",
            id = id ?? 1,
            method,
            @params = @params ?? new { },
        });
        return new StringContent(body, Encoding.UTF8, "application/json");
    }

    private static async Task<JsonElement> SendRpc(HttpClient http, string method, object? @params = null, object? id = null)
    {
        var resp = await http.PostAsync("api/mcp", JsonRpc(method, @params, id));
        resp.EnsureSuccessStatusCode();
        var json = await resp.Content.ReadAsStringAsync();
        return JsonDocument.Parse(json).RootElement;
    }

    // -- Initialize --------------------------------------------------

    [Fact]
    public async Task Initialize_Returns_Protocol_Version()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "initialize", new
        {
            protocolVersion = "2025-11-25",
            capabilities = new { },
            clientInfo = new { name = "test", version = "1.0" },
        });

        var proto = result.GetProperty("result").GetProperty("protocolVersion").GetString();
        Assert.Equal("2025-11-25", proto);

        var name = result.GetProperty("result").GetProperty("serverInfo").GetProperty("name").GetString();
        Assert.Equal("traverse", name);
    }

    [Fact]
    public async Task Initialized_Notification_Returns_202()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var resp = await http.PostAsync("api/mcp", JsonRpc("notifications/initialized"), TestContext.Current.CancellationToken);

        Assert.Equal(HttpStatusCode.Accepted, resp.StatusCode);
    }

    [Fact]
    public async Task Get_Returns_405()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var resp = await http.GetAsync("api/mcp", TestContext.Current.CancellationToken);

        Assert.Equal(HttpStatusCode.MethodNotAllowed, resp.StatusCode);
    }

    [Fact]
    public async Task Delete_Returns_405()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var resp = await http.DeleteAsync("api/mcp", TestContext.Current.CancellationToken);

        Assert.Equal(HttpStatusCode.MethodNotAllowed, resp.StatusCode);
    }

    // -- Tools list --------------------------------------------------

    [Fact]
    public async Task ToolsList_Returns_All_Tools()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/list");
        var tools = result.GetProperty("result").GetProperty("tools");

        Assert.Equal(JsonValueKind.Array, tools.ValueKind);
        var names = tools.EnumerateArray()
            .Select(t => t.GetProperty("name").GetString())
            .ToHashSet();

        Assert.Contains("query", names);
        Assert.Contains("schema", names);
        Assert.Contains("databases", names);
        Assert.Contains("search_nodes", names);
        Assert.Contains("shortest_path", names);
        Assert.Contains("neighbors", names);
        Assert.Equal(6, names.Count);
    }

    [Fact]
    public async Task ToolsList_Tools_Have_InputSchema()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/list");
        var tools = result.GetProperty("result").GetProperty("tools");

        foreach (var tool in tools.EnumerateArray())
        {
            Assert.True(tool.TryGetProperty("inputSchema", out var schema),
                $"Tool '{tool.GetProperty("name")}' missing inputSchema");
            Assert.Equal("object", schema.GetProperty("type").GetString());
        }
    }

    // -- Query tool --------------------------------------------------

    [Fact]
    public async Task Query_Returns_Scalar()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new { cypher = "RETURN 42 AS n", database = _fixture.TestDatabase },
        });

        var content = GetToolText(result);
        var data = JsonDocument.Parse(content).RootElement;

        Assert.Equal(1, data.GetProperty("total_rows").GetInt32());
        Assert.False(data.GetProperty("truncated").GetBoolean());
        var row0 = data.GetProperty("rows")[0];
        Assert.Equal(42, row0[0].GetInt32());
    }

    [Fact]
    public async Task Query_With_Parameters()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new
            {
                cypher = "RETURN $x + $y AS sum",
                database = _fixture.TestDatabase,
                parameters = new { x = 10, y = 32 },
            },
        });

        var content = GetToolText(result);
        var data = JsonDocument.Parse(content).RootElement;
        Assert.Equal(42, data.GetProperty("rows")[0][0].GetInt32());
    }

    [Fact]
    public async Task Query_Write_And_Read()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();
        var db = _fixture.TestDatabase;

        try
        {
            // Write
            await SendRpc(http, "tools/call", new
            {
                name = "query",
                arguments = new { cypher = "CREATE (n:_McpTest {val: 99}) RETURN n", database = db },
            });

            // Read back
            var result = await SendRpc(http, "tools/call", new
            {
                name = "query",
                arguments = new { cypher = "MATCH (n:_McpTest) RETURN n.val AS v", database = db },
            });

            var content = GetToolText(result);
            var data = JsonDocument.Parse(content).RootElement;
            Assert.Equal(99, data.GetProperty("rows")[0][0].GetInt32());
        }
        finally
        {
            await SendRpc(http, "tools/call", new
            {
                name = "query",
                arguments = new { cypher = "MATCH (n:_McpTest) DELETE n", database = db },
            });
        }
    }

    [Fact]
    public async Task Query_Syntax_Error_Returns_IsError()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new { cypher = "NOT VALID CYPHER", database = _fixture.TestDatabase },
        });

        var content = result.GetProperty("result").GetProperty("content");
        Assert.True(result.GetProperty("result").GetProperty("isError").GetBoolean());
    }

    // -- Schema tool -------------------------------------------------

    [Fact]
    public async Task Schema_Returns_Labels()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();
        var db = _fixture.TestDatabase;

        await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new { cypher = "CREATE (n:_McpSchemaLabel {x: 1})", database = db },
        });

        try
        {
            var result = await SendRpc(http, "tools/call", new
            {
                name = "schema",
                arguments = new { database = db },
            });

            var content = GetToolText(result);
            Assert.Contains("_McpSchemaLabel", content);
        }
        finally
        {
            await SendRpc(http, "tools/call", new
            {
                name = "query",
                arguments = new { cypher = "MATCH (n:_McpSchemaLabel) DELETE n", database = db },
            });
        }
    }

    // -- Databases tool ----------------------------------------------

    [Fact]
    public async Task Databases_Lists_Test_Database()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new
        {
            name = "databases",
            arguments = new { },
        });

        var content = GetToolText(result);
        Assert.Contains(_fixture.TestDatabase, content);
    }

    // -- Search nodes tool -------------------------------------------

    [Fact]
    public async Task SearchNodes_Finds_By_Label_And_Property()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();
        var db = _fixture.TestDatabase;

        await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new { cypher = "CREATE (:_McpSearch {tag: 'findme'})", database = db },
        });

        try
        {
            var result = await SendRpc(http, "tools/call", new
            {
                name = "search_nodes",
                arguments = new { label = "_McpSearch", property = "tag", value = "findme", database = db },
            });

            var content = GetToolText(result);
            var data = JsonDocument.Parse(content).RootElement;
            Assert.True(data.GetProperty("count").GetInt32() >= 1);
            Assert.Contains("findme", content);
        }
        finally
        {
            await SendRpc(http, "tools/call", new
            {
                name = "query",
                arguments = new { cypher = "MATCH (n:_McpSearch) DELETE n", database = db },
            });
        }
    }

    // -- Neighbors tool ----------------------------------------------

    [Fact]
    public async Task Neighbors_Returns_Connected_Nodes()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();
        var db = _fixture.TestDatabase;

        // Create two connected nodes and get the source ID
        var createResult = await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new
            {
                cypher = "CREATE (a:_McpNbr {role:'src'})-[:_MCP_LINK]->(b:_McpNbr {role:'dst'}) RETURN id(a) AS aid",
                database = db,
            },
        });

        try
        {
            var createData = JsonDocument.Parse(GetToolText(createResult)).RootElement;
            var nodeId = createData.GetProperty("rows")[0][0].GetInt64();

            var result = await SendRpc(http, "tools/call", new
            {
                name = "neighbors",
                arguments = new { node_id = nodeId, direction = "outgoing", database = db },
            });

            var content = GetToolText(result);
            var data = JsonDocument.Parse(content).RootElement;
            Assert.True(data.GetProperty("count").GetInt32() >= 1);
            Assert.Contains("_MCP_LINK", content);
        }
        finally
        {
            await SendRpc(http, "tools/call", new
            {
                name = "query",
                arguments = new { cypher = "MATCH (n:_McpNbr) DETACH DELETE n", database = db },
            });
        }
    }

    // -- Ping --------------------------------------------------------

    [Fact]
    public async Task Ping_Returns_Empty_Result()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "ping");
        Assert.Equal(JsonValueKind.Object, result.GetProperty("result").ValueKind);
    }

    // -- Error handling ----------------------------------------------

    [Fact]
    public async Task Unknown_Method_Returns_Error()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "nonexistent/method");
        var err = result.GetProperty("error");
        Assert.Equal(-32601, err.GetProperty("code").GetInt32());
    }

    [Fact]
    public async Task Unknown_Tool_Returns_IsError()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new
        {
            name = "nonexistent_tool",
            arguments = new { },
        });

        Assert.True(result.GetProperty("result").GetProperty("isError").GetBoolean());
    }

    [Fact]
    public async Task Missing_Tool_Name_Returns_Error()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new { arguments = new { } });
        var err = result.GetProperty("error");
        Assert.Equal(-32602, err.GetProperty("code").GetInt32());
    }

    [Fact]
    public async Task Invalid_Json_Returns_Parse_Error()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var resp = await http.PostAsync("api/mcp", new StringContent("{bad json", Encoding.UTF8, "application/json"), TestContext.Current.CancellationToken);
        resp.EnsureSuccessStatusCode(); // JSON-RPC returns 200 even for errors

        var json = await resp.Content.ReadAsStringAsync(TestContext.Current.CancellationToken);
        var doc = JsonDocument.Parse(json).RootElement;
        Assert.Equal(-32700, doc.GetProperty("error").GetProperty("code").GetInt32());
    }

    // -- Auth --------------------------------------------------------

    [Fact]
    public async Task No_Auth_Returns_Unauthorized()
    {
        _fixture.RequireServer();
        Assert.SkipWhen(_fixture.ApiKey is null, "Server does not require auth");

        using var http = CreateHttpClient(withAuth: false);
        var resp = await http.PostAsync("api/mcp", JsonRpc("initialize"), TestContext.Current.CancellationToken);

        Assert.Equal(HttpStatusCode.Unauthorized, resp.StatusCode);
    }

    // -- Database not found ------------------------------------------

    [Fact]
    public async Task Query_Unknown_Database_Returns_IsError()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "tools/call", new
        {
            name = "query",
            arguments = new { cypher = "RETURN 1", database = "nonexistent_db_xyz" },
        });

        Assert.True(result.GetProperty("result").GetProperty("isError").GetBoolean());
        var text = GetToolText(result);
        Assert.Contains("not found", text);
    }

    // -- JSON-RPC ID preservation ------------------------------------

    [Fact]
    public async Task Response_Preserves_Request_Id()
    {
        _fixture.RequireServer();
        using var http = CreateHttpClient();

        var result = await SendRpc(http, "ping", id: 42);
        Assert.Equal(42, result.GetProperty("id").GetInt32());

        var result2 = await SendRpc(http, "ping", id: "my-string-id");
        Assert.Equal("my-string-id", result2.GetProperty("id").GetString());
    }

    // -- Helper to extract tool text content -------------------------

    private static string GetToolText(JsonElement rpcResponse)
    {
        return rpcResponse
            .GetProperty("result")
            .GetProperty("content")[0]
            .GetProperty("text")
            .GetString()!;
    }
}
