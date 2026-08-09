using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Windows.Automation;

internal sealed record HostRequest(
    string Id,
    string Method,
    string? CapabilityToken,
    JsonElement? Params,
    string? Application,
    string? Intent,
    string? Target);

internal static class Program
{
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);
    private static readonly string ExpectedToken = Environment.GetEnvironmentVariable("ALFRED_CAPABILITY_TOKEN") ?? "";
    private static readonly string[] DestructiveWords = ["delete", "remove", "erase", "trash", "purge", "wipe", "shred", "overwrite", "empty recycle"];
    private static readonly IReadOnlyDictionary<string, string> LaunchTargets = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase)
    {
        ["Notepad"] = "notepad.exe",
        ["Calculator"] = "calc.exe",
        ["Paint"] = "mspaint.exe",
        ["File Explorer"] = "explorer.exe",
        ["Microsoft Edge"] = "msedge.exe",
        ["Google Chrome"] = "chrome.exe",
        ["Brave"] = "brave.exe"
    };

    [STAThread]
    private static async Task Main(string[] args)
    {
        if (args.Any(value => value.StartsWith("chrome-extension://", StringComparison.OrdinalIgnoreCase)) || args.Contains("--browser-bridge"))
        {
            await RunBrowserBridge();
            return;
        }
        string? line;
        while ((line = await Console.In.ReadLineAsync()) is not null)
        {
            HostRequest? request = null;
            try
            {
                request = JsonSerializer.Deserialize<HostRequest>(line, Json) ?? throw new InvalidOperationException("Empty request");
                Authorize(request);
                var result = Dispatch(request);
                Reply(new { id = request.Id, ok = true, result });
            }
            catch (Exception error)
            {
                Reply(new { id = request?.Id ?? "unknown", ok = false, error = error.Message });
            }
        }
    }

    private static async Task RunBrowserBridge()
    {
        var root = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Alfred");
        Directory.CreateDirectory(root);
        var tokenPath = Path.Combine(root, "browser-bridge-token");
        if (!File.Exists(tokenPath)) await File.WriteAllTextAsync(tokenPath, Convert.ToHexString(RandomNumberGenerator.GetBytes(32)));
        var token = (await File.ReadAllTextAsync(tokenPath)).Trim();
        var listener = new TcpListener(IPAddress.Loopback, 17844);
        listener.Start(1);
        while (true)
        {
            using var client = await listener.AcceptTcpClientAsync();
            using var network = client.GetStream();
            using var reader = new StreamReader(network, leaveOpen: true);
            using var writer = new StreamWriter(network, leaveOpen: true) { AutoFlush = true };
            var line = await reader.ReadLineAsync();
            if (line is null) continue;
            using var envelope = JsonDocument.Parse(line);
            if (!envelope.RootElement.TryGetProperty("capabilityToken", out var supplied) || supplied.GetString() != token)
            {
                await writer.WriteLineAsync("{\"ok\":false,\"error\":\"Invalid browser bridge token\"}");
                continue;
            }
            var request = envelope.RootElement.GetProperty("request").GetRawText();
            var bytes = System.Text.Encoding.UTF8.GetBytes(request);
            var output = Console.OpenStandardOutput();
            await output.WriteAsync(BitConverter.GetBytes(bytes.Length));
            await output.WriteAsync(bytes);
            await output.FlushAsync();
            var input = Console.OpenStandardInput();
            var header = new byte[4];
            await input.ReadExactlyAsync(header);
            var response = new byte[BitConverter.ToInt32(header)];
            await input.ReadExactlyAsync(response);
            await writer.WriteLineAsync(System.Text.Encoding.UTF8.GetString(response));
        }
    }

    private static void Authorize(HostRequest request)
    {
        if (string.IsNullOrWhiteSpace(ExpectedToken) || request.CapabilityToken != ExpectedToken)
            throw new UnauthorizedAccessException("Invalid or missing Alfred capability token.");
        var text = $"{request.Method} {request.Intent} {request.Target} {request.Params}".ToLowerInvariant();
        if (DestructiveWords.Any(text.Contains))
            throw new UnauthorizedAccessException("Destructive actions are blocked by the Windows host.");
    }

    private static object Dispatch(HostRequest request) => request.Method switch
    {
        "health" => new { host = "windows", version = "0.1.1", processId = Environment.ProcessId },
        "listApplications" => ListApplications(),
        "launchApplication" => LaunchApplication(request),
        "focusApplication" => FocusApplication(request),
        "observeWindow" => ObserveWindow(ResolveProcess(request).Id),
        "captureWindow" => CaptureWindow(ResolveProcess(request).Id),
        "invokeElement" => InvokeElement(request),
        "click" => Click(request, GetInt(request.Params, "x"), GetInt(request.Params, "y")),
        "typeText" => TypeText(request, GetString(request.Params, "text")),
        "key" => PressKey(request, GetInt(request.Params, "virtualKey")),
        _ => throw new InvalidOperationException($"Unsupported host method: {request.Method}")
    };

    private static object ListApplications() => Process.GetProcesses()
        .Where(p => p.MainWindowHandle != IntPtr.Zero)
        .Select(p => new { processId = p.Id, name = p.ProcessName, title = p.MainWindowTitle })
        .OrderBy(p => p.name).ToArray();

    private static object LaunchApplication(HostRequest request)
    {
        var application = GetApplication(request);
        if (!LaunchTargets.TryGetValue(application, out var executable))
            throw new UnauthorizedAccessException($"Alfred is not allowed to launch {application}.");
        var process = Process.Start(new ProcessStartInfo(executable) { UseShellExecute = true })
            ?? throw new InvalidOperationException($"Windows could not launch {application}.");
        try { process.WaitForInputIdle(5000); } catch { }
        for (var attempt = 0; attempt < 40 && process.MainWindowHandle == IntPtr.Zero; attempt++)
        {
            Thread.Sleep(100);
            process.Refresh();
        }
        if (process.MainWindowHandle == IntPtr.Zero)
            process = FindApplicationProcess(application)
                ?? throw new InvalidOperationException($"{application} launched without an accessible window.");
        FocusProcess(process);
        return new { launched = true, application, processId = process.Id, title = process.MainWindowTitle };
    }

    private static object FocusApplication(HostRequest request)
    {
        var process = ResolveProcess(request);
        FocusProcess(process);
        return new { focused = true, application = GetApplication(request), processId = process.Id, title = process.MainWindowTitle };
    }

    private static void FocusProcess(Process process)
    {
        process.Refresh();
        if (process.MainWindowHandle == IntPtr.Zero)
            throw new InvalidOperationException("The application does not have an accessible window.");
        ShowWindow(process.MainWindowHandle, SW_RESTORE);
        var root = AutomationElement.FromHandle(process.MainWindowHandle);
        var focused = false;
        try { root?.SetFocus(); focused = root is not null; } catch { }
        if (!SetForegroundWindow(process.MainWindowHandle) && !focused)
            throw new InvalidOperationException("Windows would not focus the requested application.");
        Thread.Sleep(150);
    }

    private static Process ResolveProcess(HostRequest request)
    {
        var processId = GetOptionalInt(request.Params, "processId");
        if (processId.HasValue)
            return Process.GetProcessById(processId.Value);
        return FindApplicationProcess(GetApplication(request))
            ?? throw new InvalidOperationException($"{GetApplication(request)} is not open.");
    }

    private static Process? FindApplicationProcess(string application)
    {
        LaunchTargets.TryGetValue(application, out var executable);
        var expectedName = Path.GetFileNameWithoutExtension(executable ?? application);
        return Process.GetProcesses()
            .Where(process => process.MainWindowHandle != IntPtr.Zero)
            .OrderByDescending(process => process.Id)
            .FirstOrDefault(process =>
                process.ProcessName.Equals(expectedName, StringComparison.OrdinalIgnoreCase)
                || process.MainWindowTitle.Contains(application, StringComparison.OrdinalIgnoreCase));
    }

    private static string GetApplication(HostRequest request) =>
        string.IsNullOrWhiteSpace(request.Application)
            ? GetOptionalString(request.Params, "application")
                ?? throw new InvalidOperationException("Missing application.")
            : request.Application.Trim();

    private static object ObserveWindow(int processId)
    {
        var process = Process.GetProcessById(processId);
        var root = AutomationElement.FromHandle(process.MainWindowHandle)
            ?? throw new InvalidOperationException("The application window is unavailable.");
        return Snapshot(root, 0);
    }

    private static object Snapshot(AutomationElement element, int depth)
    {
        var properties = element.Current;
        var children = depth >= 5 ? [] : element.FindAll(TreeScope.Children, Condition.TrueCondition)
            .Cast<AutomationElement>().Take(250).Select(item => Snapshot(item, depth + 1)).ToArray();
        var bounds = properties.BoundingRectangle;
        return new
        {
            name = properties.Name,
            automationId = properties.AutomationId,
            controlType = properties.ControlType?.ProgrammaticName,
            enabled = properties.IsEnabled,
            offscreen = properties.IsOffscreen,
            bounds = new { x = bounds.X, y = bounds.Y, width = bounds.Width, height = bounds.Height },
            children
        };
    }

    private static object CaptureWindow(int processId)
    {
        var process = Process.GetProcessById(processId);
        if (!GetWindowRect(process.MainWindowHandle, out var rect))
            throw new InvalidOperationException("Could not read the window bounds.");
        var width = Math.Max(1, rect.Right - rect.Left);
        var height = Math.Max(1, rect.Bottom - rect.Top);
        using var bitmap = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        using (var graphics = Graphics.FromImage(bitmap))
            graphics.CopyFromScreen(rect.Left, rect.Top, 0, 0, new Size(width, height), CopyPixelOperation.SourceCopy);
        using var output = new MemoryStream();
        bitmap.Save(output, ImageFormat.Png);
        return new { mimeType = "image/png", width, height, base64 = Convert.ToBase64String(output.ToArray()) };
    }

    private static object InvokeElement(HostRequest request)
    {
        var automationId = GetString(request.Params, "automationId");
        var process = ResolveProcess(request);
        var root = AutomationElement.FromHandle(process.MainWindowHandle);
        var element = root.FindFirst(TreeScope.Descendants,
            new PropertyCondition(AutomationElement.AutomationIdProperty, automationId))
            ?? throw new InvalidOperationException("The requested UI element was not found.");
        if (!element.TryGetCurrentPattern(InvokePattern.Pattern, out var pattern))
            throw new InvalidOperationException("The UI element is not invokable.");
        ((InvokePattern)pattern).Invoke();
        return new { invoked = true, automationId };
    }

    private static object Click(HostRequest request, int x, int y)
    {
        if (!string.IsNullOrWhiteSpace(request.Application))
            FocusProcess(ResolveProcess(request));
        SetCursorPos(x, y);
        Send([MouseInput(MOUSEEVENTF_LEFTDOWN), MouseInput(MOUSEEVENTF_LEFTUP)]);
        return new { clicked = true, x, y };
    }

    private static object TypeText(HostRequest request, string text)
    {
        FocusProcess(ResolveProcess(request));
        foreach (var character in text)
            Send([KeyboardInput(character, false), KeyboardInput(character, true)]);
        return new { typed = true, characters = text.Length };
    }

    private static object PressKey(HostRequest request, int virtualKey)
    {
        if (virtualKey == VK_DELETE)
            throw new UnauthorizedAccessException("The Delete key is blocked by Alfred's deletion policy.");
        FocusProcess(ResolveProcess(request));
        Send([VirtualKeyInput((ushort)virtualKey, false), VirtualKeyInput((ushort)virtualKey, true)]);
        return new { pressed = true, virtualKey };
    }

    private static int GetInt(JsonElement? value, string property) =>
        value?.GetProperty(property).GetInt32() ?? throw new InvalidOperationException($"Missing {property}.");
    private static int? GetOptionalInt(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) && item.TryGetInt32(out var result) ? result : null;
    private static string GetString(JsonElement? value, string property) =>
        value?.GetProperty(property).GetString() ?? throw new InvalidOperationException($"Missing {property}.");
    private static string? GetOptionalString(JsonElement? value, string property) =>
        value.HasValue && value.Value.TryGetProperty(property, out var item) ? item.GetString() : null;
    private static void Reply(object value) { Console.Out.WriteLine(JsonSerializer.Serialize(value, Json)); Console.Out.Flush(); }

    private static INPUT MouseInput(uint flags) => new() { type = INPUT_MOUSE, U = new InputUnion { mi = new MOUSEINPUT { dwFlags = flags } } };
    private static INPUT KeyboardInput(char value, bool up) => new() { type = INPUT_KEYBOARD, U = new InputUnion { ki = new KEYBDINPUT { wScan = value, dwFlags = KEYEVENTF_UNICODE | (up ? KEYEVENTF_KEYUP : 0) } } };
    private static INPUT VirtualKeyInput(ushort value, bool up) => new() { type = INPUT_KEYBOARD, U = new InputUnion { ki = new KEYBDINPUT { wVk = value, dwFlags = up ? KEYEVENTF_KEYUP : 0 } } };
    private static void Send(INPUT[] inputs)
    {
        if (SendInput((uint)inputs.Length, inputs, Marshal.SizeOf<INPUT>()) != inputs.Length)
            throw new InvalidOperationException("Windows rejected the input event.");
    }

    private const uint INPUT_MOUSE = 0, INPUT_KEYBOARD = 1, MOUSEEVENTF_LEFTDOWN = 0x0002, MOUSEEVENTF_LEFTUP = 0x0004;
    private const uint KEYEVENTF_KEYUP = 0x0002, KEYEVENTF_UNICODE = 0x0004;
    private const int VK_DELETE = 0x2E;
    private const int SW_RESTORE = 9;
    [StructLayout(LayoutKind.Sequential)] private struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] private struct INPUT { public uint type; public InputUnion U; }
    [StructLayout(LayoutKind.Explicit)] private struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
    [StructLayout(LayoutKind.Sequential)] private struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] private struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo; }
    [DllImport("user32.dll")] private static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] private static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] private static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] private static extern bool ShowWindow(IntPtr hWnd, int command);
    [DllImport("user32.dll", SetLastError = true)] private static extern uint SendInput(uint count, INPUT[] inputs, int size);
}
