/** Thousands separators, as the app's Rust side writes them ("2,100"). */
export function thousands(n: number): string {
  return Math.round(n).toLocaleString("en-US");
}

/**
 * A model's name from the provider's model list ("Claude Sonnet 5.5"), else its id. The list comes
 * from the service (for "Claude (sign in)" / "Codex (sign in)": Selene's own labels).
 */
export function modelName(models: readonly { id: string; name: string }[], id: string | null | undefined): string | null {
  if (!id) return null;
  return models.find((m) => m.id === id)?.name ?? id;
}
