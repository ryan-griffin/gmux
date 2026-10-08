/* A GTK/Pango-only reduction of renderer-check --cache-renderer selected.
 * Build with cc -Wall -Wextra -Werror -O0 -g ... $(pkg-config --cflags --libs gtk4) -lm.
 * No application caches, frame callbacks, or compositor capture are involved.
 */
#include <gtk/gtk.h>
#include <math.h>
#include <string.h>

static char *output;
static char *sequence;
static gboolean no_prelude, no_context, no_churn, same_node, keep_node;
static GskRenderNode *kept;
static int rounds = 100;
static int pairs;

static GskRenderNode *
scene (PangoContext *context, const char *fontname, double scale, double ox, double oy,
       gboolean dark, int step, int churn)
{
  GtkSnapshot *snapshot = gtk_snapshot_new ();
  gtk_snapshot_scale (snapshot, scale, scale);
  graphene_point_t origin = GRAPHENE_POINT_INIT (ox, oy);
  gtk_snapshot_translate (snapshot, &origin);
  GdkRGBA bg = dark ? (GdkRGBA){ .12, .12, .13, 1 } : (GdkRGBA){ 1, 1, 1, 1 };
  GdkRGBA fg = dark ? (GdkRGBA){ 1, 1, 1, 1 } : (GdkRGBA){ .12, .12, .13, 1 };
  graphene_rect_t rect = GRAPHENE_RECT_INIT (0, 0, 400, 168);
  gtk_snapshot_append_color (snapshot, &bg, &rect);
  const char *clusters[]
      = { "A", "m", "e\314\201", "界", "👩‍💻", "🙂", "א", "م", "\356\202\260", "j" };
  PangoFontDescription *font = pango_font_description_from_string (fontname);
  for (int y = 0; y < 8; y++)
    {
      PangoFontDescription *row_font = pango_font_description_copy (font);
      pango_font_description_set_weight (row_font, y % 4 == 1 || y % 4 == 3 ? PANGO_WEIGHT_BOLD
                                                                            : PANGO_WEIGHT_NORMAL);
      pango_font_description_set_style (row_font,
                                        y % 4 >= 2 ? PANGO_STYLE_ITALIC : PANGO_STYLE_NORMAL);
      for (int x = 0; x < 40; x++)
        {
          char text[8];
          const char *cluster = clusters[(x + y + step) % G_N_ELEMENTS (clusters)];
          if (churn)
            {
              int length = g_unichar_to_utf8 (0x4e00 + churn * 320 + y * 40 + x, text);
              text[length] = 0;
              cluster = text;
            }
          PangoLayout *layout = pango_layout_new (context);
          pango_layout_set_font_description (layout, row_font);
          pango_layout_set_text (layout, cluster, -1);
          gtk_snapshot_save (snapshot);
          graphene_point_t point = GRAPHENE_POINT_INIT (x * 10, y * 21);
          gtk_snapshot_translate (snapshot, &point);
          if (y == 4)
            gtk_snapshot_push_opacity (snapshot, .5);
          gtk_snapshot_append_layout (snapshot, layout, &fg);
          if (y == 4)
            gtk_snapshot_pop (snapshot);
          gtk_snapshot_restore (snapshot);
          g_object_unref (layout);
          if (!churn
              && (strcmp (cluster, "界") == 0 || strcmp (cluster, "👩‍💻") == 0
                  || strcmp (cluster, "🙂") == 0))
            x++;
        }
      pango_font_description_free (row_font);
    }
  pango_font_description_free (font);
  return gtk_snapshot_free_to_node (snapshot);
}

static GdkTexture *
render (GskRenderer *renderer, GskRenderNode *node, double scale)
{
  graphene_rect_t viewport = GRAPHENE_RECT_INIT (0, 0, ceil (400 * scale), ceil (168 * scale));
  return gsk_renderer_render_texture (renderer, node, &viewport);
}

static guchar *
pixels (GdkTexture *texture)
{
  int width = gdk_texture_get_width (texture), height = gdk_texture_get_height (texture);
  guchar *data = g_malloc (width * height * 4);
  gdk_texture_download (texture, data, width * 4);
  return data;
}

static char *
artifact (const char *name)
{
  return g_build_filename (output, name, NULL);
}

static void
write_text (const char *name, const char *text)
{
  g_autofree char *path = artifact (name);
  g_autoptr (GError) error = NULL;
  if (!g_file_set_contents (path, text, -1, &error))
    g_error ("%s: %s", path, error->message);
}

/* Diagnostics run only after failure. They never change its exit status. */
static void
mismatch (GskRenderer *renderer, GskRenderNode *a, GskRenderNode *b, GdkTexture *ta, GdkTexture *tb,
          const guchar *pa, const guchar *pb, double scale, double ox, double oy, int step)
{
  int width = gdk_texture_get_width (ta), height = gdk_texture_get_height (ta);
  int count = 0, left = width, top = height, right = -1, bottom = -1, delta = 0;
  GString *samples = g_string_new ("");
  for (int i = 0; i < width * height; i++)
    {
      if (memcmp (pa + i * 4, pb + i * 4, 4) == 0)
        continue;
      int x = i % width, y = i / width;
      left = MIN (left, x);
      top = MIN (top, y);
      right = MAX (right, x);
      bottom = MAX (bottom, y);
      for (int channel = 0; channel < 4; channel++)
        delta = MAX (delta, abs (pa[i * 4 + channel] - pb[i * 4 + channel]));
      if (count++ < 8)
        {
          const guchar *p = pa + i * 4, *q = pb + i * 4;
          g_string_append_printf (
              samples,
              "%s{\"x\":%d,\"y\":%d,\"actual_bgra\":[%u,%u,%u,%u],\"expected_bgra\":[%u,%u,%u,%u]}",
              samples->len ? "," : "", x, y, p[0], p[1], p[2], p[3], q[0], q[1], q[2], q[3]);
        }
    }
  g_autoptr (GBytes) ba = gsk_render_node_serialize (a), bb = gsk_render_node_serialize (b);
  gboolean nodes_equal = g_bytes_equal (ba, bb), downloads_stable = TRUE;
  gboolean renders_equal[3], actual_matches[3], expected_matches[3];
  for (int i = 0; i < 3; i++)
    {
      g_autofree guchar *again_a = pixels (ta), *again_b = pixels (tb);
      downloads_stable &= memcmp (pa, again_a, width * height * 4) == 0
                          && memcmp (pb, again_b, width * height * 4) == 0;
      GdkTexture *repeat_a = render (renderer, a, scale), *repeat_b = render (renderer, b, scale);
      g_autofree guchar *ra = pixels (repeat_a), *rb = pixels (repeat_b);
      renders_equal[i] = memcmp (ra, rb, width * height * 4) == 0;
      actual_matches[i] = memcmp (ra, pa, width * height * 4) == 0;
      expected_matches[i] = memcmp (rb, pb, width * height * 4) == 0;
      g_object_unref (repeat_a);
      g_object_unref (repeat_b);
    }
  g_autofree char *report = g_strdup_printf (
      "{\n  \"sequence\":\"%s\", \"same_node\":%s, \"keep_node\":%s, \"pair\":%d, "
      "\"gtk_renderer\":\"%s\",\n"
      "  \"size\":[%d,%d], \"scale\":%g, \"origin\":[%g,%g], \"step\":%d,\n"
      "  \"nodes_equal\":%s, \"differing_pixels\":%d, \"bounds\":[%d,%d,%d,%d], "
      "\"max_channel_delta\":%d,\n"
      "  \"first\":[%s], \"three_downloads_stable\":%s, "
      "\"diagnostic_render_pairs_equal\":[%s,%s,%s],\n"
      "  \"diagnostic_actual_matches_original\":[%s,%s,%s], "
      "\"diagnostic_expected_matches_original\":[%s,%s,%s]\n}\n",
      sequence, same_node ? "true" : "false", keep_node ? "true" : "false", pairs,
      G_OBJECT_TYPE_NAME (renderer), width, height, scale, ox, oy, step,
      nodes_equal ? "true" : "false", count, left, top, right, bottom, delta, samples->str,
      downloads_stable ? "true" : "false", renders_equal[0] ? "true" : "false",
      renders_equal[1] ? "true" : "false", renders_equal[2] ? "true" : "false",
      actual_matches[0] ? "true" : "false", actual_matches[1] ? "true" : "false",
      actual_matches[2] ? "true" : "false", expected_matches[0] ? "true" : "false",
      expected_matches[1] ? "true" : "false", expected_matches[2] ? "true" : "false");
  write_text ("mismatch.json", report);
  const char *names[] = { "actual", "expected" };
  GdkTexture *textures[] = { ta, tb };
  GskRenderNode *nodes[] = { a, b };
  for (int i = 0; i < 2; i++)
    {
      g_autofree char *png_name = g_strconcat (names[i], ".png", NULL),
                      *node_name = g_strconcat (names[i], ".node", NULL);
      g_autofree char *png_path = artifact (png_name), *node_path = artifact (node_name);
      g_autoptr (GError) error = NULL;
      if (!gdk_texture_save_to_png (textures[i], png_path))
        g_error ("Could not save %s", png_path);
      if (!gsk_render_node_write_to_file (nodes[i], node_path, &error))
        g_error ("%s: %s", node_path, error->message);
    }
  g_print ("FAIL %s pair %d: %d pixels, nodes_equal=%s; %s/mismatch.json\n",
           G_OBJECT_TYPE_NAME (renderer), pairs, count, nodes_equal ? "true" : "false", output);
  g_string_free (samples, TRUE);
}

static gboolean
compare (GskRenderer *renderer, PangoContext *context, const char *font, double scale, double ox,
         double oy, int step)
{
  if (keep_node && !kept)
    kept = scene (context, font, scale, ox, oy, TRUE, step, 0);
  GskRenderNode *a = keep_node ? gsk_render_node_ref (kept)
                               : scene (context, font, scale, ox, oy, TRUE, step, 0);
  GskRenderNode *b
      = same_node ? gsk_render_node_ref (a) : scene (context, font, scale, ox, oy, TRUE, step, 0);
  GdkTexture *ta = render (renderer, a, scale);
  g_autofree guchar *pa = pixels (ta);
  GdkTexture *tb = render (renderer, b, scale);
  g_autofree guchar *pb = pixels (tb);
  pairs++;
  /* The viewport fixes dimensions. Check them before indexing either buffer. */
  if (gdk_texture_get_width (ta) != gdk_texture_get_width (tb)
      || gdk_texture_get_height (ta) != gdk_texture_get_height (tb))
    g_error ("Equivalent scenes returned different texture dimensions");
  gboolean equal
      = memcmp (pa, pb, gdk_texture_get_width (ta) * gdk_texture_get_height (ta) * 4) == 0;
  if (!equal)
    mismatch (renderer, a, b, ta, tb, pa, pb, scale, ox, oy, step);
  g_object_unref (ta);
  g_object_unref (tb);
  gsk_render_node_unref (a);
  gsk_render_node_unref (b);
  return equal;
}

static gboolean
run (GskRenderer *renderer, PangoContext *context)
{
  if (strcmp (sequence, "full") == 0)
    {
      const char *fonts[]
          = { "Monospace 9", "Monospace 12", "Monospace Bold Italic 16", "Monospace 14px" };
      if (!no_prelude)
        for (int dark = 0; dark < 2; dark++)
          for (int f = 0; f < 4; f++)
            for (int sc = 0; sc < 2; sc++)
              {
                double scale = sc ? 1.5 : 1;
                GskRenderNode *node = scene (context, fonts[f], scale, .25, .5, dark, 0, 0);
                GdkTexture *texture = render (renderer, node, scale);
                g_autofree guchar *data = pixels (texture);
                g_object_unref (texture);
                gsk_render_node_unref (node);
              }
      for (int step = 0; step < 19; step++)
        {
          if (step == 12 && !no_context)
            pango_context_changed (context);
          if (!compare (renderer, context, step >= 11 ? "Monospace 16" : "Monospace 12",
                        step >= 13 ? 1.5 : 1, step >= 14 ? .25 : 0, step >= 14 ? .5 : 0, 0))
            return FALSE;
        }
      if (!no_churn)
        for (int i = 0; i < 16; i++)
          {
            GskRenderNode *node = scene (context, "Monospace 16", 1, 0, 0, TRUE, 0, i + 1);
            gsk_render_node_unref (node);
          }
    }
  if (strcmp (sequence, "fixed") == 0)
    return compare (renderer, context, "Monospace 12", 1, 0, 0, 0);
  const double scales[] = { 1, 1.25, 1.5, 2 };
  for (int sc = 0; sc < 4; sc++)
    for (int origin = 0; origin < 2; origin++)
      for (int step = 0; step < 7; step++)
        {
          if (!compare (renderer, context, "Monospace 12", scales[sc], origin ? .25 : 0,
                        origin ? .5 : 0, step))
            return FALSE;
        }
  return TRUE;
}

int
main (int argc, char **argv)
{
  GOptionEntry entries[]
      = { { "output", 0, 0, G_OPTION_ARG_FILENAME, &output, "Artifact directory", "DIR" },
          { "sequence", 0, 0, G_OPTION_ARG_STRING, &sequence, "full, scroll, or fixed", "NAME" },
          { "rounds", 0, 0, G_OPTION_ARG_INT, &rounds,
            "Bounded sequence repetitions in this process", "N" },
          { "keep-node", 0, 0, G_OPTION_ARG_NONE, &keep_node,
            "Build the fixed scene once and reuse it for all pairs", NULL },
          { "same-node", 0, 0, G_OPTION_ARG_NONE, &same_node,
            "Render the same node object twice per pair", NULL },
          { "no-prelude", 0, 0, G_OPTION_ARG_NONE, &no_prelude,
            "Skip the full sequence's font/theme renders", NULL },
          { "no-context", 0, 0, G_OPTION_ARG_NONE, &no_context,
            "Skip the full sequence's context change", NULL },
          { "no-churn", 0, 0, G_OPTION_ARG_NONE, &no_churn,
            "Skip the full sequence's CJK layout churn", NULL },
          { NULL, 0, 0, 0, NULL, NULL, NULL } };
  g_autoptr (GOptionContext) options = g_option_context_new (
      "- compare exact pixels from the same or equivalent GTK/Pango scenes");
  g_option_context_add_main_entries (options, entries, NULL);
  g_autoptr (GError) error = NULL;
  if (!g_option_context_parse (options, &argc, &argv, &error))
    {
      g_printerr ("%s\n", error->message);
      return 2;
    }
  if (!sequence)
    sequence = g_strdup ("fixed");
  if (!output)
    output = g_strdup ("artifacts/gsk-texture-repro");
  if (argc != 1 || rounds < 1 || rounds > 1000 || (keep_node && strcmp (sequence, "fixed") != 0)
      || (strcmp (sequence, "full") && strcmp (sequence, "scroll") && strcmp (sequence, "fixed")))
    {
      g_printerr (
          "Expected full, scroll, or fixed and 1..1000 rounds; --keep-node requires fixed\n");
      return 2;
    }
  if (keep_node)
    same_node = TRUE;
#if G_BYTE_ORDER != G_LITTLE_ENDIAN
  g_printerr ("This probe labels native ARGB32 download bytes as little-endian BGRA\n");
  return 2;
#endif
  if (g_mkdir_with_parents (output, 0700) != 0)
    {
      g_printerr ("Cannot create %s\n", output);
      return 2;
    }
  gtk_init ();
  GtkWidget *window = gtk_window_new ();
  gtk_window_present (GTK_WINDOW (window));
  GskRenderer *renderer = g_object_ref (gtk_native_get_renderer (GTK_NATIVE (window)));
  PangoContext *context = gtk_widget_get_pango_context (window);
  gboolean equal = TRUE;
  for (int i = 0; i < rounds && equal; i++)
    equal = run (renderer, context);
  g_autofree char *result = g_strdup_printf (
      "{\"equal\":%s,\"pairs\":%d,\"sequence\":\"%s\",\"same_node\":%s,\"keep_node\":%s,\"gtk_"
      "renderer\":\"%s\","
      "\"gdk_display\":\"%"
      "s\",\"gtk_version\":\"%u.%u.%u\",\"pango_version\":\"%s\"}\n",
      equal ? "true" : "false", pairs, sequence, same_node ? "true" : "false",
      keep_node ? "true" : "false", G_OBJECT_TYPE_NAME (renderer),
      G_OBJECT_TYPE_NAME (gtk_widget_get_display (window)), gtk_get_major_version (),
      gtk_get_minor_version (), gtk_get_micro_version (), pango_version_string ());
  write_text ("result.json", result);
  if (equal)
    g_print ("PASS %s: %d exact pairs\n", G_OBJECT_TYPE_NAME (renderer), pairs);
  if (kept)
    gsk_render_node_unref (kept);
  gtk_window_destroy (GTK_WINDOW (window));
  gsk_renderer_unrealize (renderer);
  g_object_unref (renderer);
  g_free (sequence);
  g_free (output);
  return equal ? 0 : 1;
}
