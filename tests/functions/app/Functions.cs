// The functions the Functions host smoke test drives: each binding below makes
// the host itself (its .NET storage SDKs) talk to the emulator's path-style
// storage ports through AzureWebJobsStorage.
using Azure.Data.Tables;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc;
using Microsoft.Azure.Functions.Worker;
using Microsoft.Extensions.Logging;

public class Row : ITableEntity
{
    public string PartitionKey { get; set; } = "";
    public string RowKey { get; set; } = "";
    public DateTimeOffset? Timestamp { get; set; }
    public Azure.ETag ETag { get; set; }
    public string Text { get; set; } = "";
}

public class Output
{
    [BlobOutput("outputs/{queueTrigger}.txt")]
    public string? Blob { get; set; }
    [TableOutput("processed")]
    public Row? Row { get; set; }
    [QueueOutput("done-items")]
    public string? Done { get; set; }
}

public class Functions(ILogger<Functions> log)
{
    // Function-level authorization: the host keeps its keys in azure-webjobs-secrets.
    [Function("Enqueue")]
    [QueueOutput("work-items")]
    public string Enqueue([HttpTrigger(AuthorizationLevel.Function, "post")] HttpRequest request)
        => request.Query["id"].ToString();

    // A queue trigger with blob, table and queue outputs; "poison" ids fail
    // until the host moves them to work-items-poison.
    [Function("Work")]
    public Output Work([QueueTrigger("work-items")] string id)
    {
        log.LogInformation("Work {id}", id);
        if (id.StartsWith("poison")) throw new InvalidOperationException("poison message");
        return new Output { Blob = $"processed {id}", Row = new Row { PartitionKey = "p", RowKey = id, Text = id }, Done = id };
    }

    [Function("Read")]
    public IActionResult Read(
        [HttpTrigger(AuthorizationLevel.Function, "get", Route = "read/{id}")] HttpRequest request,
        [BlobInput("outputs/{id}.txt")] string? blob)
        => new OkObjectResult(blob);

    // A timer: a singleton, so the host holds a blob lease for its listener.
    [Function("Tick")]
    public void Tick([TimerTrigger("*/5 * * * * *", RunOnStartup = true)] TimerInfo timer)
        => log.LogInformation("Tick");

    // A blob trigger: container scans, blob receipts and the host's blob-trigger queue.
    [Function("OnBlob")]
    [QueueOutput("blob-seen")]
    public string OnBlob([BlobTrigger("inbox/{name}", Source = BlobTriggerSource.LogsAndContainerScan)] string content, string name)
        => name;
}
