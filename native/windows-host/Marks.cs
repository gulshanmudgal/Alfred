using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.Drawing.Text;
using System.Text.Json;
using System.Windows.Automation;

internal sealed record MarkRecord
{
    public required string Id { get; init; }
    public required int ProcessId { get; init; }
    public required int Generation { get; init; }
    public required string Name { get; init; }
    public required string AutomationId { get; init; }
    public required string ControlType { get; init; }
    public required string[] Patterns { get; init; }
    public required bool Enabled { get; init; }
    public required bool Chrome { get; init; }
    public required double X { get; init; }
    public required double Y { get; init; }
    public required double Width { get; init; }
    public required double Height { get; init; }
    public string Source { get; init; } = "catalog";
}

internal static partial class Program
{
    private const int MaxMarks = 40;
    private const int MaxMarkScan = 400;
    private const int QuerySearchLimit = 12;
    private const int MaxQueryMarks = 12;
    private static readonly TimeSpan QueryBudget = TimeSpan.FromSeconds(2.5);

    private static readonly object MarkGate = new();
    private static readonly Dictionary<int, int> GenerationByProcess = [];
    private static readonly List<MarkRecord> Marks = [];

    private static readonly ControlType[] InteractiveTypes =
    [
        ControlType.Button, ControlType.Edit, ControlType.MenuItem, ControlType.ListItem,
        ControlType.Hyperlink, ControlType.TabItem, ControlType.ComboBox, ControlType.CheckBox,
        ControlType.RadioButton, ControlType.Document, ControlType.SplitButton, ControlType.TreeItem,
        ControlType.DataItem, ControlType.Spinner, ControlType.Slider, ControlType.HeaderItem,
        ControlType.Thumb, ControlType.Custom
    ];

    private static readonly string[] PersistentLossPhrases =
    [
        "empty recycle", "empty trash", "empty bin", "empty the recycle",
        "to trash", "to the trash", "to recycle", "recycle bin",
        "permanently delete", "delete permanently", "uninstall", "format drive",
        "drop table", "wipe disk", "shred", "purge records", "overwrite existing",
        "replace file"
    ];

    private static readonly string[] DestructionTargets =
    [
        "delete", "delete item", "delete file", "delete email", "delete message",
        "delete account", "delete user", "delete post", "delete mail",
        "remove user", "remove account", "remove member",
        "delete permanently", "move to recycle bin", "empty recycle bin", "empty trash"
    ];

    private static readonly string[] DestructionNouns =
    [
        "account", "user", "file", "email", "message", "item", "post", "mail",
        "row", "record", "folder", "calendar", "contact", "member", "project",
        "task", "workspace", "permanently", "data"
    ];

    private static readonly string[] ReversibleRemoveHints =
        ["filter", "draft", "selection", "highlight", "formatting"];

    private static readonly string[] ConfirmationLabels =
        ["confirm", "yes", "ok", "okay", "continue", "proceed", "accept", "apply", "i understand"];

    private static int GenerationFor(int processId)
    {
        lock (MarkGate)
            return GenerationByProcess.TryGetValue(processId, out var generation) ? generation : 0;
    }

    private static void PruneDeadMarks()
    {
        HashSet<int> live;
        try
        {
            live = Process.GetProcesses()
                .Select(process =>
                {
                    try { return process.Id; }
                    finally { process.Dispose(); }
                })
                .ToHashSet();
        }
        catch { return; }
        lock (MarkGate)
        {
            Marks.RemoveAll(mark => !live.Contains(mark.ProcessId));
            foreach (var dead in GenerationByProcess.Keys.Where(id => !live.Contains(id)).ToArray())
                GenerationByProcess.Remove(dead);
        }
    }

    private static string[] PolicyTokens(string lower) =>
        System.Text.RegularExpressions.Regex
            .Split(lower, "[^a-z0-9]+")
            .Where(token => token.Length > 0)
            .ToArray();

    private static bool IsDestructionVerb(string token) =>
        token is "delete" or "remove" or "erase" or "destroy" or "purge"
            or "uninstall" or "trash" or "overwrite" or "wipe";

    private static bool VerbIsReversible(string[] tokens, int index)
    {
        if (tokens[index] is not ("delete" or "remove" or "erase")) return false;
        if (tokens.Any(DestructionNouns.Contains)) return false;
        var window = tokens.Skip(index).Take(4).ToArray();
        if (!window.Any(ReversibleRemoveHints.Contains)) return false;
        if (window.Contains("draft")
            && !window.Any(token => token is "text" or "selection" or "highlight" or "formatting"))
            return false;
        return true;
    }

    internal static bool IsDestructionLabel(string? name)
    {
        var raw = (name ?? "").Trim();
        if (raw.Length == 0) return false;
        var lower = raw.ToLowerInvariant();
        if (PersistentLossPhrases.Any(lower.Contains)) return true;
        var normalized = NormalizeText(raw);
        if (DestructionTargets.Contains(normalized)) return true;
        var tokens = PolicyTokens(lower);
        for (var index = 0; index < tokens.Length; index++)
        {
            if (IsDestructionVerb(tokens[index]) && !VerbIsReversible(tokens, index))
                return true;
        }
        return false;
    }

    private static bool IsConfirmationLabel(string? name)
    {
        var normalized = NormalizeText(name);
        return ConfirmationLabels.Contains(normalized);
    }

    private static List<string> AncestorNames(AutomationElement element, int depth)
    {
        var names = new List<string>();
        var current = element;
        for (var i = 0; i < depth && current is not null; i++)
        {
            try
            {
                var name = current.Current.Name ?? "";
                if (!string.IsNullOrWhiteSpace(name)) names.Add(name);
                current = TreeWalker.ControlViewWalker.GetParent(current);
            }
            catch { break; }
        }
        return names;
    }

    private static bool LiveControlIsDestructive(AutomationElement element)
    {
        try
        {
            var names = AncestorNames(element, 6);
            var self = names.FirstOrDefault() ?? element.Current.Name ?? "";
            if (IsDismissiveLabel(self)) return false;
            if (names.Any(IsDestructionLabel)) return true;
            var joined = string.Join(" ", names);
            if (IsDestructionLabel(joined)) return true;
            if (IsConfirmationLabel(self)
                && names.Skip(1).Any(name => IsDestructionLabel(name) || IsDestructionLabel($"{name} {self}")))
                return true;
            var lower = joined.ToLowerInvariant();
            return lower.Contains("recycle") && names.Any(name =>
                name.Contains("empty", StringComparison.OrdinalIgnoreCase));
        }
        catch { return false; }
    }

    private static void RefuseDestructiveControl(AutomationElement? element)
    {
        if (element is not null && LiveControlIsDestructive(element))
            throw new UnauthorizedAccessException("Destructive actions are blocked by the Windows host.");
    }

    private static readonly string[] DismissiveLabels =
        ["cancel", "no", "close", "dismiss", "back", "never mind", "not now", "keep"];

    private static bool IsDismissiveLabel(string? name)
    {
        var normalized = NormalizeText(name);
        return DismissiveLabels.Contains(normalized);
    }

    private static bool IsTrustedPagePoint(HostRequest request)
    {
        var space = (GetOptionalString(request.Params, "space") ?? "").ToLowerInvariant();
        return space is "page" or "viewport";
    }

    private static void RefuseUnverifiedBrowserCoordinate(HostRequest request, AutomationElement? semanticTarget)
    {
        if (semanticTarget is not null) return;
        if (IsTrustedPagePoint(request)) return;
        if (!BrowserApplications.Contains(GetApplication(request))) return;
        if (string.IsNullOrWhiteSpace(request.Target)) return;
        throw new InvalidOperationException(
            $"No enabled visible browser page control matches '{request.Target}'. Alfred will not fall back to an unverified browser coordinate.");
    }

    private static bool IsPersistentDataLoss(string? method, string? intent, string? target, JsonElement? payload)
    {
        var methodName = (method ?? "").ToLowerInvariant();
        var mutating = methodName is "click" or "invokeelement" or "rightclick" or "doubleclick" or "drag";
        if (mutating && (IsDestructionLabel(target) || IsDestructionLabel(intent)))
            return true;
        var targetText = (target ?? "").ToLowerInvariant();
        if (PersistentLossPhrases.Any(targetText.Contains))
            return true;
        var intentText = (intent ?? "").ToLowerInvariant();
        if (PersistentLossPhrases.Any(intentText.Contains))
            return true;
        if (payload.HasValue && payload.Value.ValueKind == JsonValueKind.Object)
        {
            foreach (var property in payload.Value.EnumerateObject())
            {
                if (property.NameEquals("text") || property.NameEquals("value") || property.NameEquals("url")
                    || property.NameEquals("mark") || property.NameEquals("from") || property.NameEquals("to")
                    || property.NameEquals("generation") || property.NameEquals("processId")
                    || property.NameEquals("nx") || property.NameEquals("ny")
                    || property.NameEquals("x") || property.NameEquals("y"))
                    continue;
                var raw = property.Value.ValueKind == JsonValueKind.String
                    ? property.Value.GetString() ?? ""
                    : property.Value.ToString();
                if (IsDestructionLabel(raw) || PersistentLossPhrases.Any(raw.ToLowerInvariant().Contains))
                    return true;
            }
        }
        return false;
    }

    private static object BuildObservation(Process process, string application)
    {
        var handle = RequireWindowHandle(process, application);
        var root = AutomationElement.FromHandle(handle)
            ?? throw new InvalidOperationException("The application window is unavailable.");
        var catalog = CollectMarks(process, application, root);
        var snippets = CollectTextSnippets(root, 12);
        List<MarkRecord> published;
        int generation;
        lock (MarkGate)
        {
            var previousQuery = Marks
                .Where(mark => mark.ProcessId == process.Id && mark.Source == "query")
                .ToList();
            generation = GenerationFor(process.Id) + 1;
            GenerationByProcess[process.Id] = generation;
            Marks.RemoveAll(mark => mark.ProcessId == process.Id);
            var used = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            var nextIndex = 1;
            string NextCatalogId()
            {
                string id;
                do { id = $"n{nextIndex++}"; } while (!used.Add(id));
                return id;
            }
            foreach (var mark in catalog)
            {
                var prior = previousQuery.FirstOrDefault(old => SameFingerprint(old, mark));
                var id = prior is not null && used.Add(prior.Id) ? prior.Id : NextCatalogId();
                Marks.Add(mark with { Id = id, ProcessId = process.Id, Generation = generation, Source = "catalog" });
            }
            var retained = 0;
            foreach (var old in previousQuery)
            {
                if (retained >= MaxQueryMarks) break;
                if (Marks.Any(mark => mark.ProcessId == process.Id && SameFingerprint(mark, old)))
                    continue;
                if (!QueryMarkStillLive(process, old)) continue;
                var id = used.Add(old.Id) ? old.Id : NextCatalogId();
                Marks.Add(old with { Id = id, Generation = generation, Source = "query" });
                retained++;
            }
            published = Marks.Where(mark => mark.ProcessId == process.Id).ToList();
        }
        string? focused = null;
        try
        {
            var focus = AutomationElement.FocusedElement;
            if (focus is not null && ElementBelongsToProcess(focus, process, application))
                focused = published.FirstOrDefault(mark => SameControl(mark, focus))?.Id;
        }
        catch { /* Focus can race while the target redraws. */ }
        uint dpi = 96;
        try { dpi = GetDpiForWindow(handle); } catch { /* Older Windows. */ }
        return new
        {
            generation,
            application,
            processId = process.Id,
            title = Truncate(WindowTitle(handle), 160),
            dpi,
            focused,
            markCount = published.Count,
            marks = published.Select(MarkJson).ToArray(),
            texts = snippets.ToArray()
        };
    }

    private static List<MarkRecord> CollectMarks(Process process, string application, AutomationElement root)
    {
        var candidates = new List<(MarkRecord Mark, int Score, AutomationElement Element)>();
        var remaining = MaxMarkScan;
        WalkInteractive(root, process, application, candidates, ref remaining, 0);
        try
        {
            var focus = AutomationElement.FocusedElement;
            if (focus is not null && ElementBelongsToProcess(focus, process, application)
                && !candidates.Any(item => SameControl(item.Mark, focus)))
            {
                var mark = DescribeMark("n0", process, application, focus);
                candidates.Insert(0, (mark, 10_000, focus));
            }
        }
        catch { /* Ignore a missing focused element. */ }
        return candidates
            .OrderByDescending(item => item.Score)
            .Take(MaxMarks)
            .Select((item, index) => item.Mark with { Id = $"n{index + 1}" })
            .ToList();
    }

    private static void WalkInteractive(
        AutomationElement element,
        Process process,
        string application,
        List<(MarkRecord Mark, int Score, AutomationElement Element)> into,
        ref int remaining,
        int depth)
    {
        if (remaining <= 0 || depth > 12) return;
        remaining--;
        try
        {
            if (IsMarkCandidate(element))
            {
                var mark = DescribeMark("n?", process, application, element);
                into.Add((mark, ScoreMark(element, mark), element));
            }
        }
        catch { /* Stale UIA node. */ }
        if (depth >= 12) return;
        AutomationElement? child;
        try { child = TreeWalker.ControlViewWalker.GetFirstChild(element); }
        catch { return; }
        var seen = 0;
        while (child is not null && remaining > 0 && seen < MaxSnapshotChildren)
        {
            WalkInteractive(child, process, application, into, ref remaining, depth + 1);
            seen++;
            try { child = TreeWalker.ControlViewWalker.GetNextSibling(child); }
            catch { break; }
        }
    }

    private static bool IsMarkCandidate(AutomationElement element)
    {
        if (!IsUsableElement(element)) return false;
        var current = element.Current;
        var type = current.ControlType;
        if (type == ControlType.Window || type == ControlType.Pane || type == ControlType.Thumb)
            return false;
        var named = !string.IsNullOrWhiteSpace(current.Name) || !string.IsNullOrWhiteSpace(current.AutomationId);
        var interactive = InteractiveTypes.Contains(type)
            || HasPattern(element, InvokePattern.Pattern)
            || HasPattern(element, ValuePattern.Pattern)
            || HasPattern(element, TogglePattern.Pattern)
            || HasPattern(element, SelectionItemPattern.Pattern)
            || HasPattern(element, ExpandCollapsePattern.Pattern);
        if (type == ControlType.Custom && !named && !HasPattern(element, InvokePattern.Pattern))
            return false;
        if (type == ControlType.Text) return false;
        return interactive && (named || type == ControlType.Edit || type == ControlType.Document);
    }

    private static int ScoreMark(AutomationElement element, MarkRecord mark)
    {
        var score = 0;
        try
        {
            if (AutomationElement.FocusedElement is { } focus && SameControl(mark, focus)) score += 1000;
        }
        catch { /* Focus query is best-effort. */ }
        if (mark.Enabled) score += 50;
        if (!mark.Chrome) score += 40;
        if (!string.IsNullOrWhiteSpace(mark.Name)) score += 30;
        if (mark.Patterns.Contains("Invoke") || mark.Patterns.Contains("Value")) score += 40;
        if (mark.ControlType is "ControlType.Button" or "ControlType.Edit" or "ControlType.Hyperlink") score += 20;
        score += (int)Math.Min(30, mark.Width * mark.Height / 2000);
        return score;
    }

    private static MarkRecord DescribeMark(string id, Process process, string application, AutomationElement element)
    {
        var current = element.Current;
        var bounds = current.BoundingRectangle;
        var chrome = BrowserApplications.Contains(application) && IsBrowserChromeControl(element, process);
        return new MarkRecord
        {
            Id = id,
            ProcessId = process.Id,
            Generation = GenerationFor(process.Id),
            Name = current.Name ?? "",
            AutomationId = current.AutomationId ?? "",
            ControlType = current.ControlType?.ProgrammaticName ?? "",
            Patterns = DetectPatterns(element),
            Enabled = current.IsEnabled,
            Chrome = chrome,
            X = JsonNumber(bounds.X),
            Y = JsonNumber(bounds.Y),
            Width = JsonNumber(bounds.Width),
            Height = JsonNumber(bounds.Height)
        };
    }

    private static string[] DetectPatterns(AutomationElement element)
    {
        var patterns = new List<string>();
        if (HasPattern(element, InvokePattern.Pattern)) patterns.Add("Invoke");
        if (HasPattern(element, ValuePattern.Pattern)) patterns.Add("Value");
        if (HasPattern(element, TextPattern.Pattern)) patterns.Add("Text");
        if (HasPattern(element, TogglePattern.Pattern)) patterns.Add("Toggle");
        if (HasPattern(element, ExpandCollapsePattern.Pattern)) patterns.Add("ExpandCollapse");
        if (HasPattern(element, SelectionItemPattern.Pattern)) patterns.Add("SelectionItem");
        if (HasPattern(element, ScrollPattern.Pattern)) patterns.Add("Scroll");
        if (HasPattern(element, ScrollItemPattern.Pattern)) patterns.Add("ScrollItem");
        return patterns.ToArray();
    }

    private static bool HasPattern(AutomationElement element, AutomationPattern pattern)
    {
        try { return element.TryGetCurrentPattern(pattern, out _); }
        catch { return false; }
    }

    private static bool SameControl(MarkRecord mark, AutomationElement element)
    {
        try
        {
            var current = element.Current;
            if (current.ProcessId != mark.ProcessId)
            {
                try
                {
                    using var owner = Process.GetProcessById(mark.ProcessId);
                    if (!ElementBelongsToProcess(element, owner)) return false;
                }
                catch { return false; }
            }
            var type = current.ControlType?.ProgrammaticName ?? "";
            if (type != mark.ControlType) return false;
            var name = current.Name ?? "";
            if (name != mark.Name) return false;
            if (!string.IsNullOrWhiteSpace(mark.AutomationId)
                && !mark.AutomationId.Equals(current.AutomationId ?? "", StringComparison.Ordinal))
                return false;
            var bounds = current.BoundingRectangle;
            return Math.Abs(mark.X - bounds.X) < 12
                && Math.Abs(mark.Y - bounds.Y) < 12
                && Math.Abs(mark.Width - bounds.Width) < 24
                && Math.Abs(mark.Height - bounds.Height) < 24;
        }
        catch { return false; }
    }

    private static object MarkJson(MarkRecord mark) => new
    {
        id = mark.Id,
        role = mark.ControlType.Replace("ControlType.", ""),
        name = TruncateSnapshotText(mark.Name),
        automationId = TruncateSnapshotText(mark.AutomationId),
        patterns = mark.Patterns,
        enabled = mark.Enabled,
        chrome = mark.Chrome,
        source = mark.Source,
        generation = mark.Generation
    };

    private static bool SameFingerprint(MarkRecord left, MarkRecord right) =>
        left.ProcessId == right.ProcessId
        && left.ControlType == right.ControlType
        && left.AutomationId == right.AutomationId
        && left.Name == right.Name;

    private static bool DocumentLooksLikeComposer(AutomationElement document)
    {
        try
        {
            var current = document.Current;
            var identity = $"{current.Name} {current.AutomationId}".ToLowerInvariant();
            var writable = document.TryGetCurrentPattern(ValuePattern.Pattern, out var value)
                && value is ValuePattern pattern
                && !pattern.Current.IsReadOnly;
            if (writable
                || identity.Contains("compose")
                || identity.Contains("editor")
                || identity.Contains("tweet")
                || identity.Contains("message")
                || identity.Contains("post text"))
                return true;

            // Chromium contenteditable surfaces are often Document + no ValuePattern
            // and an empty name. Treat a keyboard-focusable Document as a composer
            // only when it occupies a fraction of the nearest window/pane — not the
            // page or Word document surface whose text *is* the published body.
            if (!current.IsKeyboardFocusable) return false;
            var docRect = current.BoundingRectangle;
            if (docRect.Width <= 0 || docRect.Height <= 0) return false;
            var ancestor = TreeWalker.ControlViewWalker.GetParent(document);
            for (var hop = 0; hop < 8 && ancestor is not null; hop++)
            {
                var ancestorType = ancestor.Current.ControlType;
                if (ancestorType == ControlType.Window || ancestorType == ControlType.Pane)
                {
                    var win = ancestor.Current.BoundingRectangle;
                    var docArea = Math.Max(1.0, docRect.Width * docRect.Height);
                    var winArea = Math.Max(1.0, win.Width * win.Height);
                    return docArea < winArea * 0.55;
                }
                ancestor = TreeWalker.ControlViewWalker.GetParent(ancestor);
            }
        }
        catch { /* Stale UIA node. */ }
        return false;
    }

    private static bool InsideComposer(AutomationElement element)
    {
        var current = element;
        for (var depth = 0; depth < 8 && current is not null; depth++)
        {
            try
            {
                var type = current.Current.ControlType;
                if (type == ControlType.Edit) return true;
                if (type == ControlType.Document && DocumentLooksLikeComposer(current))
                    return true;
                current = TreeWalker.ControlViewWalker.GetParent(current);
            }
            catch { return false; }
        }
        return false;
    }

    private static bool QueryMarkStillLive(Process process, MarkRecord mark)
    {
        try
        {
            if (!string.IsNullOrWhiteSpace(mark.AutomationId) || !string.IsNullOrWhiteSpace(mark.Name))
            {
                var resolved = FindElement(process,
                    string.IsNullOrWhiteSpace(mark.AutomationId) ? null : mark.AutomationId,
                    string.IsNullOrWhiteSpace(mark.Name) ? null : mark.Name,
                    string.IsNullOrWhiteSpace(mark.ControlType) ? null : mark.ControlType);
                if (SameControl(mark, resolved)) return true;
            }
            var hit = HitTest((int)Math.Round(mark.X + mark.Width / 2), (int)Math.Round(mark.Y + mark.Height / 2));
            return hit is not null && SameControl(mark, hit);
        }
        catch { return false; }
    }

    private static string NextMarkId(int processId)
    {
        var highest = 0;
        foreach (var mark in Marks.Where(item => item.ProcessId == processId))
        {
            if (mark.Id.Length > 1
                && mark.Id.StartsWith('n')
                && int.TryParse(mark.Id[1..], out var number))
                highest = Math.Max(highest, number);
        }
        return $"n{highest + 1}";
    }

    private static List<string> CollectTextSnippets(AutomationElement root, int max)
    {
        var snippets = new List<string>();
        var remaining = 800;
        CollectTextWalk(root, snippets, ref remaining, 0, max);
        return snippets;
    }

    private static void CollectTextWalk(
        AutomationElement element, List<string> into, ref int remaining, int depth, int max)
    {
        if (into.Count >= max || remaining <= 0 || depth > 14) return;
        remaining--;
        try
        {
            var current = element.Current;
            if (current.ControlType == ControlType.Text
                && !string.IsNullOrWhiteSpace(current.Name)
                && current.Name.Trim().Length >= 8
                && !InsideComposer(element))
            {
                var snippet = Truncate(current.Name.Trim(), 160);
                if (!into.Contains(snippet)) into.Add(snippet);
            }
        }
        catch { /* Stale text node. */ }
        AutomationElement? child;
        try { child = TreeWalker.ControlViewWalker.GetFirstChild(element); }
        catch { return; }
        var seen = 0;
        while (child is not null && into.Count < max && remaining > 0 && seen < 80)
        {
            CollectTextWalk(child, into, ref remaining, depth + 1, max);
            seen++;
            try { child = TreeWalker.ControlViewWalker.GetNextSibling(child); }
            catch { break; }
        }
    }

    private static MarkRecord MintMark(Process process, string application, AutomationElement element)
    {
        lock (MarkGate)
        {
            if (GenerationFor(process.Id) == 0)
                GenerationByProcess[process.Id] = 1;
            var existing = Marks.FirstOrDefault(mark =>
                mark.ProcessId == process.Id && SameControl(mark, element));
            if (existing is not null) return existing;
            var mark = DescribeMark(NextMarkId(process.Id), process, application, element) with
            {
                Generation = GenerationFor(process.Id),
                Source = "query"
            };
            Marks.Add(mark);
            return mark;
        }
    }

    private static List<MarkRecord> FindMarksByText(Process process, string application, string text)
    {
        var needle = text.Trim();
        if (needle.Length == 0) return [];
        if (GenerationFor(process.Id) == 0)
            BuildObservation(process, application);
        var matches = new List<MarkRecord>();
        lock (MarkGate)
        {
            matches.AddRange(Marks.Where(mark =>
                mark.ProcessId == process.Id
                && (mark.Name.Contains(needle, StringComparison.OrdinalIgnoreCase)
                    || mark.AutomationId.Contains(needle, StringComparison.OrdinalIgnoreCase))));
        }
        if (matches.Count >= QuerySearchLimit) return matches.Take(QuerySearchLimit).ToList();
        foreach (var element in SearchTreeByText(process, application, needle, QuerySearchLimit, QueryBudget))
        {
            matches.Add(MintMark(process, application, element));
            if (matches.Count >= QuerySearchLimit) break;
        }
        return matches
            .GroupBy(mark => mark.Id, StringComparer.OrdinalIgnoreCase)
            .Select(group => group.First())
            .Take(QuerySearchLimit)
            .ToList();
    }

    private static List<AutomationElement> SearchTreeByText(
        Process process, string application, string needle, int limit, TimeSpan budget)
    {
        var root = RequireAutomationRoot(process, application);
        var found = new List<AutomationElement>();
        var deadline = DateTime.UtcNow + budget;
        try
        {
            var exact = root.FindAll(TreeScope.Descendants,
                new PropertyCondition(AutomationElement.NameProperty, needle));
            foreach (AutomationElement element in exact.Cast<AutomationElement>())
            {
                found.Add(element);
                if (found.Count >= limit) return found;
            }
        }
        catch { /* Name-equals FindAll is best-effort. */ }

        var stack = new Stack<AutomationElement>();
        stack.Push(root);
        while (stack.Count > 0 && found.Count < limit && DateTime.UtcNow < deadline)
        {
            var element = stack.Pop();
            try
            {
                var current = element.Current;
                var name = current.Name ?? "";
                var automationId = current.AutomationId ?? "";
                var value = "";
                try
                {
                    if (element.TryGetCurrentPattern(ValuePattern.Pattern, out var pattern)
                        && pattern is ValuePattern typed)
                        value = typed.Current.Value ?? "";
                }
                catch { /* ValuePattern is optional. */ }
                if ((name.Contains(needle, StringComparison.OrdinalIgnoreCase)
                    || automationId.Contains(needle, StringComparison.OrdinalIgnoreCase)
                    || value.Contains(needle, StringComparison.OrdinalIgnoreCase))
                    && !found.Contains(element))
                    found.Add(element);
                var child = TreeWalker.ControlViewWalker.GetFirstChild(element);
                while (child is not null)
                {
                    stack.Push(child);
                    child = TreeWalker.ControlViewWalker.GetNextSibling(child);
                }
            }
            catch { /* Skip a stale node and keep searching. */ }
        }
        return found;
    }

    private static AutomationElement? TryResolveMark(HostRequest request)
    {
        var id = GetOptionalString(request.Params, "mark");
        if (string.IsNullOrWhiteSpace(id)) return null;
        return ResolveMark(request, id);
    }

    private static AutomationElement ResolveMark(HostRequest request, string id)
    {
        var process = ResolveProcess(request);
        MarkRecord mark;
        lock (MarkGate)
        {
            mark = Marks.FirstOrDefault(item =>
                    item.ProcessId == process.Id
                    && item.Id.Equals(id, StringComparison.OrdinalIgnoreCase))
                ?? throw new InvalidOperationException($"Mark {id} is unknown for this window. Observe or find again.");
            var current = GenerationFor(process.Id);
            var requested = GetOptionalInt(request.Params, "generation");
            if (requested is int expected && expected != mark.Generation && mark.Source != "query")
                throw new InvalidOperationException($"Mark {id} expired (generation {mark.Generation} != {expected}). Observe again.");
            if (mark.Source != "query" && mark.Generation != current)
                throw new InvalidOperationException($"Mark {id} expired (generation {mark.Generation} != {current}). Observe again.");
        }
        if (!string.IsNullOrWhiteSpace(mark.AutomationId) || !string.IsNullOrWhiteSpace(mark.Name))
        {
            try
            {
                var resolved = FindElement(process,
                    string.IsNullOrWhiteSpace(mark.AutomationId) ? null : mark.AutomationId,
                    string.IsNullOrWhiteSpace(mark.Name) ? null : mark.Name,
                    string.IsNullOrWhiteSpace(mark.ControlType) ? null : mark.ControlType);
                if (SameControl(mark, resolved)) return resolved;
            }
            catch { /* Re-attach only when the fingerprint still matches. */ }
        }
        var x = (int)Math.Round(mark.X + mark.Width / 2);
        var y = (int)Math.Round(mark.Y + mark.Height / 2);
        var hit = HitTest(x, y);
        if (hit is not null && SameControl(mark, hit)) return hit;
        throw new InvalidOperationException($"Mark {id} could not be re-attached to the same live control. Observe or probe again.");
    }

    private static AutomationElement ResolveTargetElement(HostRequest request, bool requireMark)
    {
        var marked = TryResolveMark(request);
        if (marked is not null) return marked;
        if (requireMark)
            throw new InvalidOperationException("This action requires a mark from observe, find, or probe.");
        var process = ResolveProcess(request);
        return FindElement(process,
            GetOptionalString(request.Params, "automationId"),
            GetOptionalString(request.Params, "name"),
            GetOptionalString(request.Params, "controlType"));
    }

    private static AutomationElement? HitTest(int x, int y)
    {
        try { return AutomationElement.FromPoint(new System.Windows.Point(x, y)); }
        catch { return null; }
    }

    private static (int X, int Y) CenterOf(AutomationElement element)
    {
        var bounds = element.Current.BoundingRectangle;
        return ((int)Math.Round(bounds.Left + bounds.Width / 2), (int)Math.Round(bounds.Top + bounds.Height / 2));
    }

    private static bool TryNormalizedPoint(HostRequest request, Process process, out int x, out int y)
    {
        x = 0;
        y = 0;
        var nx = GetOptionalDouble(request.Params, "nx");
        var ny = GetOptionalDouble(request.Params, "ny");
        if (nx is null || ny is null) return false;
        if (nx < 0 || nx > 1 || ny < 0 || ny > 1)
            throw new InvalidOperationException("Normalized click points must be between 0 and 1 (window bitmap space).");
        if (!GetWindowRect(FindBestWindowHandle(process, GetApplication(request)), out var rect))
            throw new InvalidOperationException("Could not read the target window bounds.");
        var width = Math.Max(1, rect.Right - rect.Left);
        var height = Math.Max(1, rect.Bottom - rect.Top);
        x = rect.Left + (int)Math.Round(nx.Value * width);
        y = rect.Top + (int)Math.Round(ny.Value * height);
        RequireInsideWindow(process.Id, x, y);
        return true;
    }

    private static bool TryPageDocumentBounds(Process process, out RECT page)
    {
        page = default;
        var root = TryAutomationRoot(process, null);
        if (root is null) return false;
        AutomationElement? best = null;
        var bestArea = 0.0;
        try
        {
            var documents = root.FindAll(TreeScope.Descendants,
                new PropertyCondition(AutomationElement.ControlTypeProperty, ControlType.Document));
            foreach (AutomationElement candidate in documents.Cast<AutomationElement>())
            {
                try
                {
                    if (!IsUsableElement(candidate) || IsBrowserChromeControl(candidate, process)) continue;
                    var bounds = candidate.Current.BoundingRectangle;
                    var area = bounds.Width * bounds.Height;
                    if (area > bestArea)
                    {
                        best = candidate;
                        bestArea = area;
                    }
                }
                catch { /* Stale document node. */ }
            }
        }
        catch { return false; }
        if (best is null) return false;
        var box = best.Current.BoundingRectangle;
        page = new RECT
        {
            Left = (int)Math.Round(box.Left),
            Top = (int)Math.Round(box.Top),
            Right = (int)Math.Round(box.Right),
            Bottom = (int)Math.Round(box.Bottom)
        };
        return page.Right > page.Left && page.Bottom > page.Top;
    }

    private static bool TryPageOrWindowPoint(HostRequest request, Process process, out int x, out int y)
    {
        x = 0;
        y = 0;
        var nx = GetOptionalDouble(request.Params, "nx");
        var ny = GetOptionalDouble(request.Params, "ny");
        if (nx is null || ny is null) return false;
        if (nx < 0 || nx > 1 || ny < 0 || ny > 1)
            throw new InvalidOperationException("Normalized click points must be between 0 and 1.");
        var space = (GetOptionalString(request.Params, "space") ?? "window").ToLowerInvariant();
        RECT rect;
        if ((space is "page" or "viewport") && BrowserApplications.Contains(GetApplication(request)))
        {
            if (!TryPageDocumentBounds(process, out rect))
                return TryNormalizedPoint(request, process, out x, out y);
        }
        else if (!GetWindowRect(FindBestWindowHandle(process, GetApplication(request)), out rect))
        {
            throw new InvalidOperationException("Could not read the target window bounds.");
        }
        var width = Math.Max(1, rect.Right - rect.Left);
        var height = Math.Max(1, rect.Bottom - rect.Top);
        x = rect.Left + (int)Math.Round(nx.Value * width);
        y = rect.Top + (int)Math.Round(ny.Value * height);
        RequireInsideWindow(process.Id, x, y);
        return true;
    }

    private static object InvokeViaPatterns(Process process, AutomationElement element, string? target)
    {
        RefuseDestructiveControl(element);
        string targetName;
        string controlType;
        bool enabled;
        try
        {
            var current = element.Current;
            targetName = current.Name ?? "";
            controlType = current.ControlType?.ProgrammaticName ?? "";
            enabled = current.IsEnabled;
        }
        catch
        {
            throw new InvalidOperationException($"The requested {target ?? "control"} disappeared before it could be invoked.");
        }
        if (!enabled)
            throw new InvalidOperationException($"The requested {target ?? "control"} is disabled; its prerequisite state has not been met.");
        if (element.TryGetCurrentPattern(InvokePattern.Pattern, out var invoke))
        {
            ((InvokePattern)invoke).Invoke();
            return new { invoked = true, how = "InvokePattern", targetName };
        }
        if (element.TryGetCurrentPattern(TogglePattern.Pattern, out var toggle))
        {
            ((TogglePattern)toggle).Toggle();
            return new { invoked = true, how = "TogglePattern", targetName };
        }
        if (element.TryGetCurrentPattern(ExpandCollapsePattern.Pattern, out var expand))
        {
            var pattern = (ExpandCollapsePattern)expand;
            if (pattern.Current.ExpandCollapseState == ExpandCollapseState.Collapsed) pattern.Expand();
            else if (pattern.Current.ExpandCollapseState == ExpandCollapseState.Expanded) pattern.Collapse();
            else pattern.Expand();
            return new { invoked = true, how = "ExpandCollapsePattern", targetName };
        }
        if (element.TryGetCurrentPattern(SelectionItemPattern.Pattern, out var select))
        {
            ((SelectionItemPattern)select).Select();
            return new { invoked = true, how = "SelectionItemPattern", targetName };
        }
        var (x, y) = CenterOf(element);
        RequireInsideWindow(process.Id, x, y);
        HumanLeftClick(process.Id, x, y);
        return new { invoked = true, how = "humanClick", x, y, targetName, controlType };
    }

    private static object Probe(HostRequest request)
    {
        var process = ResolveProcess(request);
        if (!TryPageOrWindowPoint(request, process, out var x, out var y))
            throw new InvalidOperationException("probe requires nx and ny in window bitmap space (0–1).");
        var hit = HitTest(x, y);
        if (hit is null || !ElementBelongsToProcess(hit, process, GetApplication(request)))
            return new { kind = "visualOnly", x, y, reason = "No UI Automation control is under that point." };
        if (!IsUsableElement(hit)
            || hit.Current.ControlType == ControlType.Window
            || hit.Current.ControlType == ControlType.Pane
            || hit.Current.ControlType == ControlType.Document)
            return new { kind = "visualOnly", x, y, reason = "The hit is a canvas or empty pane, not a named control." };
        var mark = MintMark(process, GetApplication(request), hit);
        return new
        {
            kind = "mark",
            mark = mark.Id,
            generation = mark.Generation,
            name = mark.Name,
            role = mark.ControlType.Replace("ControlType.", ""),
            patterns = mark.Patterns,
            x,
            y
        };
    }

    private static bool IsWheelSensitive(AutomationElement element)
    {
        try
        {
            var type = element.Current.ControlType;
            return type == ControlType.ComboBox
                || type == ControlType.Spinner
                || type == ControlType.Slider
                || type == ControlType.Edit;
        }
        catch { return true; }
    }

    private static object Scroll(HostRequest request)
    {
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var direction = (GetOptionalString(request.Params, "direction") ?? "down").ToLowerInvariant();
        var text = GetOptionalString(request.Params, "text");
        var target = TryResolveMark(request);
        if (target is null && !string.IsNullOrWhiteSpace(text))
        {
            var matches = FindMarksByText(process, GetApplication(request), text);
            if (matches.Count > 0)
                target = ResolveMark(request, matches[0].Id);
        }
        if (target is not null && target.TryGetCurrentPattern(ScrollItemPattern.Pattern, out var scrollItem))
        {
            ((ScrollItemPattern)scrollItem).ScrollIntoView();
            return new { scrolled = true, how = "ScrollItemPattern" };
        }
        var pane = target ?? TryAutomationRoot(process, null);
        while (pane is not null)
        {
            if (pane.TryGetCurrentPattern(ScrollPattern.Pattern, out var scrollPattern))
            {
                var scroll = (ScrollPattern)scrollPattern;
                var horizontal = direction is "left" or "right";
                var axisAvailable = false;
                try
                {
                    axisAvailable = horizontal
                        ? scroll.Current.HorizontallyScrollable
                        : scroll.Current.VerticallyScrollable;
                }
                catch { axisAvailable = false; }
                if (!axisAvailable)
                {
                    try { pane = TreeWalker.ControlViewWalker.GetParent(pane); }
                    catch { break; }
                    continue;
                }
                var amount = direction is "up" or "left" ? ScrollAmount.LargeDecrement : ScrollAmount.LargeIncrement;
                if (horizontal) scroll.ScrollHorizontal(amount);
                else scroll.ScrollVertical(amount);
                return new { scrolled = true, how = "ScrollPattern", direction };
            }
            try { pane = TreeWalker.ControlViewWalker.GetParent(pane); }
            catch { break; }
        }
        if (!TryWheelSafePoint(process, target, out var x, out var y))
            return new { scrolled = false, how = "none", reason = "No non-value surface to wheel over." };
        RequireInsideWindow(process.Id, x, y);
        var wheelHorizontal = direction is "left" or "right";
        var delta = direction is "up" or "right" ? 360 : -360;
        HumanWheel(process.Id, x, y, delta, wheelHorizontal);
        return new { scrolled = true, how = "humanWheel", direction };
    }

    private static bool TryWheelSafePoint(Process process, AutomationElement? preferred, out int x, out int y)
    {
        var handle = FindBestWindowHandle(process, null);
        if (handle == IntPtr.Zero || !GetWindowRect(handle, out var rect))
        {
            x = 0;
            y = 0;
            return false;
        }
        var candidates = new List<(int X, int Y)>();
        if (preferred is not null && !IsWheelSensitive(preferred))
            candidates.Add(CenterOf(preferred));
        var midX = (rect.Left + rect.Right) / 2;
        var midY = (rect.Top + rect.Bottom) / 2;
        var width = Math.Max(1, rect.Right - rect.Left);
        var height = Math.Max(1, rect.Bottom - rect.Top);
        candidates.Add((midX, midY));
        candidates.Add((midX, rect.Top + height * 2 / 3));
        candidates.Add((rect.Left + width / 3, midY));
        foreach (var (cx, cy) in candidates)
        {
            if (cx < rect.Left || cx >= rect.Right || cy < rect.Top || cy >= rect.Bottom) continue;
            var hit = HitTest(cx, cy);
            if (hit is null || !ElementBelongsToProcess(hit, process) || !IsWheelSensitive(hit))
            {
                x = cx;
                y = cy;
                return true;
            }
        }
        x = 0;
        y = 0;
        return false;
    }

    private static object PointerGesture(HostRequest request, string kind)
    {
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var element = TryResolveMark(request);
        int x, y;
        if (element is not null) (x, y) = CenterOf(element);
        else if (!TryPageOrWindowPoint(request, process, out x, out y))
            throw new InvalidOperationException($"{kind} requires a mark (or nx/ny after probe).");
        else
        {
            element = HitTest(x, y);
            var named = element is not null
                && element.Current.ControlType != ControlType.Pane
                && element.Current.ControlType != ControlType.Window
                && element.Current.ControlType != ControlType.Document
                    ? element
                    : null;
            RefuseUnverifiedBrowserCoordinate(request, named);
        }
        if (kind is "rightClick" or "doubleClick")
            RefuseDestructiveControl(element ?? throw new InvalidOperationException($"{kind} has no live control under that point to safety-check."));
        RequireInsideWindow(process.Id, x, y);
        switch (kind)
        {
            case "rightClick":
                HumanRightClick(process.Id, x, y);
                break;
            case "doubleClick":
                HumanDoubleClick(process.Id, x, y);
                break;
            case "hover":
                HumanHover(process.Id, x, y);
                break;
            default:
                throw new InvalidOperationException($"Unsupported pointer gesture: {kind}");
        }
        return new { kind, how = "humanPointer", x, y, mark = GetOptionalString(request.Params, "mark") };
    }

    private static object Drag(HostRequest request)
    {
        var process = ResolveProcess(request);
        FocusProcess(process, GetApplication(request));
        var fromId = GetOptionalString(request.Params, "from") ?? GetOptionalString(request.Params, "mark");
        var toId = GetOptionalString(request.Params, "to");
        if (string.IsNullOrWhiteSpace(fromId) || string.IsNullOrWhiteSpace(toId))
            throw new InvalidOperationException("drag requires from and to marks.");
        var from = ResolveMark(request, fromId);
        var to = ResolveMark(request, toId);
        RefuseDestructiveControl(from);
        RefuseDestructiveControl(to);
        var (x1, y1) = CenterOf(from);
        var (x2, y2) = CenterOf(to);
        RequireInsideWindow(process.Id, x1, y1);
        RequireInsideWindow(process.Id, x2, y2);
        HumanDrag(process.Id, x1, y1, x2, y2);
        return new { dragged = true, how = "humanDrag", from = fromId, to = toId };
    }

    private static Bitmap AnnotateMarks(Bitmap source, Process process, RECT window)
    {
        var annotated = new Bitmap(source);
        using var graphics = Graphics.FromImage(annotated);
        graphics.SmoothingMode = SmoothingMode.AntiAlias;
        graphics.TextRenderingHint = TextRenderingHint.ClearTypeGridFit;
        using var font = new Font("Segoe UI", 9, FontStyle.Bold);
        var width = Math.Max(1, window.Right - window.Left);
        var height = Math.Max(1, window.Bottom - window.Top);
        List<MarkRecord> snapshot;
        lock (MarkGate)
        {
            var generation = GenerationFor(process.Id);
            snapshot = Marks
                .Where(mark => mark.ProcessId == process.Id && mark.Generation == generation)
                .ToList();
        }
        foreach (var mark in snapshot)
        {
            var rx = (float)((mark.X - window.Left) * annotated.Width / width);
            var ry = (float)((mark.Y - window.Top) * annotated.Height / height);
            var rw = (float)Math.Max(6, mark.Width * annotated.Width / width);
            var rh = (float)Math.Max(6, mark.Height * annotated.Height / height);
            var color = mark.Chrome
                ? Color.FromArgb(230, 196, 96, 24)
                : Color.FromArgb(230, 24, 96, 200);
            using var pen = new Pen(color, 2);
            graphics.DrawRectangle(pen, rx, ry, rw, rh);
            var badge = mark.Id.StartsWith("n", StringComparison.OrdinalIgnoreCase) ? mark.Id[1..] : mark.Id;
            var size = graphics.MeasureString(badge, font);
            var bx = Math.Clamp(rx, 0, Math.Max(0, annotated.Width - size.Width - 4));
            var by = Math.Clamp(ry - size.Height - 1, 0, Math.Max(0, annotated.Height - size.Height - 1));
            using var fill = new SolidBrush(color);
            graphics.FillRectangle(fill, bx, by, size.Width + 4, size.Height + 1);
            graphics.DrawString(badge, font, Brushes.White, bx + 2, by);
        }
        return annotated;
    }

    private static INPUT WheelInput(int delta, bool horizontal = false) => new()
    {
        type = INPUT_MOUSE,
        U = new InputUnion
        {
            mi = new MOUSEINPUT
            {
                mouseData = unchecked((uint)delta),
                dwFlags = horizontal ? MOUSEEVENTF_HWHEEL : MOUSEEVENTF_WHEEL
            }
        }
    };
}
