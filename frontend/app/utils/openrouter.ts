import type { OpenRouterModel } from "../types/models";

export type ModelFilter = "all" | "free" | "paid";

export function filterModels(
  models: OpenRouterModel[],
  query: string,
  filter: ModelFilter,
): OpenRouterModel[] {
  const normalized = query.trim().toLocaleLowerCase();
  return models.filter((model) => {
    if (filter === "free" && !model.free) return false;
    if (filter === "paid" && model.free) return false;
    return (
      !normalized ||
      `${model.id} ${model.name} ${model.description}`
        .toLocaleLowerCase()
        .includes(normalized)
    );
  });
}

export function selectionForModel(model: OpenRouterModel | undefined): {
  model: string | null;
  voice: string | null;
} {
  return { model: model?.id ?? null, voice: model?.voices[0] ?? null };
}
