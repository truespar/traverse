using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class FileTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public FileTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task ListFiles_Returns_Array()
    {
        var client = _fixture.RequireServer();
        var files = await client.ListFilesAsync(TestContext.Current.CancellationToken);

        Assert.NotNull(files);
        Assert.IsType<TvdbFileInfo[]>(files);
    }

    [Fact]
    public async Task DownloadFile_Streams_Bytes()
    {
        var client = _fixture.RequireServer();

        using var ms = new MemoryStream();
        await client.DownloadFileAsync(_fixture.TestDatabase, ms, ct: TestContext.Current.CancellationToken);

        Assert.True(ms.Length > 0, "Downloaded .tvdb should have > 0 bytes");
    }

    [Fact]
    public async Task UploadAndDeleteFile_Roundtrip()
    {
        var client = _fixture.RequireServer();
        var name = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

        // Upload a valid .tvdb
        using var factory = new TvdbFactory();
        var bytes = TvdbFactory.ReadBytes(factory.Create(name));
        var upload = await client.UploadFileAsync(name, new MemoryStream(bytes), ct: TestContext.Current.CancellationToken);
        Assert.Equal(name, upload.Name);

        try
        {
            // File should appear in list (unloaded)
            var files = await client.ListFilesAsync(TestContext.Current.CancellationToken);
            Assert.Contains(files, f => f.Name == name && !f.Loaded);
        }
        finally
        {
            // Delete the file
            await client.DeleteFileAsync(name, ct: TestContext.Current.CancellationToken);
        }

        // File should be gone
        var afterDelete = await client.ListFilesAsync(TestContext.Current.CancellationToken);
        Assert.DoesNotContain(afterDelete, f => f.Name == name);
    }
}
