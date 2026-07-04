;;; emacs_project.el --- Headless Emacs/Eglot probe for ktlsp -*- lexical-binding: t; -*-

;; Usage:
;;   emacs --batch -l dev/emacs_project.el -- \
;;     <project-dir> <file.kt> [ktlsp-bin] [highlight-needle] [semantic-burst]

(require 'cl-lib)
(require 'eglot)
(require 'jsonrpc)
(require 'kotlin-mode)
(require 'project)
(require 'subr-x)

(defvar ktlsp-harness--args
  (if (equal (car command-line-args-left) "--")
      (cdr command-line-args-left)
    command-line-args-left))
(setq command-line-args-left nil)

(defun ktlsp-harness--arg (n)
  (nth n ktlsp-harness--args))

(defun ktlsp-harness--must (value message)
  (unless (and value (not (string-empty-p value)))
    (error "%s" message))
  value)

(defvar ktlsp-harness-root
  (file-truename (ktlsp-harness--must (ktlsp-harness--arg 0)
                                      "usage: emacs --batch -l dev/emacs_project.el -- <project-dir> <file.kt> [ktlsp-bin] [highlight-needle] [semantic-burst]")))
(defvar ktlsp-harness-file
  (file-truename (ktlsp-harness--must (ktlsp-harness--arg 1)
                                      "usage: emacs --batch -l dev/emacs_project.el -- <project-dir> <file.kt> [ktlsp-bin] [highlight-needle] [semantic-burst]")))
(defvar ktlsp-harness-bin
  (or (ktlsp-harness--arg 2) (getenv "KTLSP_BIN")))
(defvar ktlsp-harness-highlight-needle
  (or (ktlsp-harness--arg 3) "KotlinLogging"))
(defvar ktlsp-harness-semantic-burst
  (max 1 (string-to-number (or (ktlsp-harness--arg 4) "6"))))

(unless (and ktlsp-harness-bin (file-exists-p ktlsp-harness-bin))
  (setq ktlsp-harness-bin
        (let* ((script (or load-file-name buffer-file-name default-directory))
               (dev-dir (file-name-directory script))
               (repo (directory-file-name (expand-file-name ".." dev-dir)))
               (release (expand-file-name "target/release/ktlsp" repo))
               (debug (expand-file-name "target/debug/ktlsp" repo)))
          (cond
           ((file-exists-p release) release)
           ((file-exists-p debug) debug)
           (t nil)))))
(unless (and ktlsp-harness-bin (file-exists-p ktlsp-harness-bin))
  (error "ktlsp binary not found"))
(unless (file-exists-p ktlsp-harness-file)
  (error "probe file not readable: %s" ktlsp-harness-file))

(defvar ktlsp-harness--failures nil)
(defvar ktlsp-harness--events nil)

(defun ktlsp-harness--record (name ok &optional detail)
  (push (list :name name :ok ok :detail detail) ktlsp-harness--events)
  (if ok
      (princ (format "PASS  %s%s\n" name (if detail (format "  (%s)" detail) "")))
    (push name ktlsp-harness--failures)
    (princ (format "FAIL  %s%s\n" name (if detail (format "  (%s)" detail) "")))))

(defun ktlsp-harness--time-ms (thunk)
  (let ((start (float-time)))
    (list :value (funcall thunk)
          :ms (* 1000.0 (- (float-time) start)))))

(defun ktlsp-harness--safe-time-ms (thunk)
  (let ((start (float-time)))
    (condition-case err
        (list :value (funcall thunk)
              :ms (* 1000.0 (- (float-time) start)))
      (error
       (list :error (error-message-string err)
             :ms (* 1000.0 (- (float-time) start)))))))

(defun ktlsp-harness--wait-for (desc pred timeout)
  (let* ((deadline (+ (float-time) timeout))
         (ok nil))
    (while (and (not ok) (< (float-time) deadline))
      (accept-process-output nil 0.1)
      (setq ok (funcall pred)))
    (ktlsp-harness--record desc ok (format "timeout=%ss" timeout))
    ok))

(defun ktlsp-harness--project-try (dir)
  (when (file-in-directory-p dir ktlsp-harness-root)
    (cons 'transient ktlsp-harness-root)))

(cl-letf (((symbol-function 'project-current)
           (lambda (&optional _maybe-prompt dir)
             (ktlsp-harness--project-try (or dir default-directory)))))
  (setq-default eglot-events-buffer-size 0)
  (setq-default eglot-sync-connect 1)
  (setq-default eglot-connect-timeout 30)
  (setq-default read-process-output-max (* 1024 1024))
  (setf (alist-get 'kotlin-mode eglot-server-programs)
        (list ktlsp-harness-bin))

  (let* ((buf (find-file-noselect ktlsp-harness-file))
         (server nil)
         (highlight-pos nil)
         (contact nil))
    (with-current-buffer buf
      (setq default-directory ktlsp-harness-root)
      (kotlin-mode)
      (setq contact (eglot--guess-contact))
      (setq server (apply #'eglot contact)))
    (ktlsp-harness--record "opened probe file" t ktlsp-harness-file)
    (ktlsp-harness--record "resolved eglot contact" (not (null contact)) (format "%S" contact))
    (ktlsp-harness--wait-for
     "eglot server initialized"
     (lambda ()
       (with-current-buffer buf
         (and (eglot-managed-p)
              server
              (eglot-current-server))))
     20)
    (with-current-buffer buf
      (setq server (eglot-current-server)))
    (let* ((server-info (and server (slot-boundp server 'server-info) (oref server server-info)))
           (caps-map (and server (slot-boundp server 'capabilities) (eglot--capabilities server))))
      (ktlsp-harness--record "server attached" (not (null server)))
      (ktlsp-harness--record "definitionProvider advertised"
                             (not (null (plist-get caps-map :definitionProvider))))
      (ktlsp-harness--record "semanticTokensProvider advertised"
                             (not (null (plist-get caps-map :semanticTokensProvider))))
      (ktlsp-harness--record "diagnosticProvider advertised"
                             (not (null (plist-get caps-map :diagnosticProvider)))
                             (when server-info (format "%s" server-info))))

    (with-current-buffer buf
      (save-excursion
        (goto-char (point-min))
        (when (search-forward ktlsp-harness-highlight-needle nil t)
          (setq highlight-pos (1- (point))))))
    (ktlsp-harness--record "found highlight anchor"
                           (numberp highlight-pos)
                           ktlsp-harness-highlight-needle)

    (let ((sem-times nil))
      (dotimes (_ ktlsp-harness-semantic-burst)
        (let* ((res
                (ktlsp-harness--safe-time-ms
                 (lambda ()
                   (jsonrpc-request
                    server
                    :textDocument/semanticTokens/full
                    `(:textDocument (:uri ,(eglot--path-to-uri ktlsp-harness-file)))))))
               (ms (plist-get res :ms))
               (err (plist-get res :error)))
          (if err
              (ktlsp-harness--record "semantic tokens request" nil (format "%.1fms %s" ms err))
            (let* ((val (plist-get res :value))
                   (count (if (and (listp val) (plist-get val :data))
                              (/ (length (plist-get val :data)) 5)
                            0)))
              (push ms sem-times)
              (ktlsp-harness--record "semantic tokens request"
                                     (> count 0)
                                     (format "%.1fms count=%s" ms count))))))

      (when highlight-pos
        (with-current-buffer buf
          (save-excursion
            (goto-char highlight-pos)
            (let* ((line (1- (line-number-at-pos)))
                   (character (current-column))
                   (res
                    (ktlsp-harness--safe-time-ms
                     (lambda ()
                       (jsonrpc-request
                        server
                        :textDocument/documentHighlight
                        `(:textDocument (:uri ,(eglot--path-to-uri ktlsp-harness-file))
                          :position (:line ,line :character ,character))))))
                   (err (plist-get res :error)))
              (if err
                  (ktlsp-harness--record "documentHighlight request" nil
                                         (format "%.1fms %s" (plist-get res :ms) err))
                (let* ((value (plist-get res :value))
                       (count (length value)))
                  (ktlsp-harness--record "documentHighlight request"
                                         (>= count 0)
                                         (format "%.1fms count=%s" (plist-get res :ms) count))))))))

      (let* ((doc-res
              (ktlsp-harness--safe-time-ms
               (lambda ()
                 (condition-case err
                     (jsonrpc-request
                      server
                      :textDocument/diagnostic
                      `(:textDocument (:uri ,(eglot--path-to-uri ktlsp-harness-file))))
                   (error (list :error (error-message-string err)))))))
             (doc-val (plist-get doc-res :value)))
        (ktlsp-harness--record "document diagnostic request"
                               (not (plist-get doc-val :error))
                               (format "%.1fms" (plist-get doc-res :ms))))

      (let* ((ws-res
              (ktlsp-harness--safe-time-ms
               (lambda ()
                 (condition-case err
                     (jsonrpc-request
                      server
                      :workspace/diagnostic
                      '(:previousResultIds []))
                   (error (list :error (error-message-string err)))))))
             (ws-val (plist-get ws-res :value)))
        (ktlsp-harness--record "workspace diagnostic request"
                               (not (plist-get ws-val :error))
                               (format "%.1fms" (plist-get ws-res :ms))))

      (when sem-times
        (let* ((vals (nreverse sem-times))
               (max-ms (apply #'max vals))
               (min-ms (apply #'min vals))
               (avg-ms (/ (apply #'+ vals) (float (length vals)))))
          (princ (format "INFO  semantic burst stats  min=%.1fms avg=%.1fms max=%.1fms n=%d\n"
                         min-ms avg-ms max-ms (length vals))))))

    (when server
      (ignore-errors (eglot-shutdown server)))
    (kill-buffer buf)))

(if ktlsp-harness--failures
    (progn
      (princ (format "\nFAILED: %s\n" (string-join (nreverse ktlsp-harness--failures) ", ")))
      (kill-emacs 1))
  (princ "\nALL PASS\n")
  (kill-emacs 0))
