import { Activity, RefreshCw } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useCheckCodexRemote, useCodexRemoteStatus } from "@/lib/query/proxy";
import { extractErrorMessage } from "@/utils/errorUtils";
import { cn } from "@/lib/utils";

export function CodexRemoteStatusRow() {
  const { t, i18n } = useTranslation();
  const query = useCodexRemoteStatus();
  const check = useCheckCodexRemote();
  const status = query.data;
  const error = query.error ?? check.error;
  const state = error ? "degraded" : (status?.state ?? "checking");
  const busy = check.isPending || state === "checking";
  const formatTime = (value: string) =>
    new Date(value).toLocaleString(i18n.language);

  return (
    <section
      aria-label={t("codexRemote.title")}
      className="border-t border-border px-4 py-3"
    >
      <div className="flex items-start gap-3">
        <Activity
          aria-hidden="true"
          className={cn(
            "mt-0.5 h-4 w-4 shrink-0",
            state === "connected"
              ? "text-green-600"
              : state === "degraded"
                ? "text-amber-600"
                : "text-muted-foreground",
          )}
        />
        <div className="min-w-0 flex-1 space-y-1">
          <p className="text-sm font-medium">{t("codexRemote.title")}</p>
          <p
            role="status"
            className="break-words text-xs text-muted-foreground"
          >
            {t(`codexRemote.states.${state}`)}
          </p>
          {status?.lastCheckedAt && (
            <p className="break-words text-xs text-muted-foreground">
              {t("codexRemote.lastChecked", {
                time: formatTime(status.lastCheckedAt),
              })}
            </p>
          )}
          {status?.nextCheckAt && state === "degraded" && (
            <p className="break-words text-xs text-muted-foreground">
              {t("codexRemote.nextCheck", {
                time: formatTime(status.nextCheckAt),
              })}
            </p>
          )}
          {(error || status?.lastError) && (
            <p role="alert" className="break-all text-xs text-amber-600">
              {error ? extractErrorMessage(error) : status?.lastError}
            </p>
          )}
        </div>
        <TooltipProvider>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                size="icon"
                variant="ghost"
                aria-label={t("codexRemote.check")}
                disabled={busy}
                onClick={() => check.mutate()}
                className="h-8 w-8 shrink-0"
              >
                <RefreshCw
                  aria-hidden="true"
                  className={cn("h-4 w-4", busy && "animate-spin")}
                />
              </Button>
            </TooltipTrigger>
            <TooltipContent>{t("codexRemote.check")}</TooltipContent>
          </Tooltip>
        </TooltipProvider>
      </div>
    </section>
  );
}
